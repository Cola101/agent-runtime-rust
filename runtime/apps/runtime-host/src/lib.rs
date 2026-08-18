//! Standalone local execution host (ADR-0035).
//!
//! Runs an Agent with no Java control plane, no PostgreSQL, no NATS, and no
//! gRPC. It links the Worker execution core as a library so the Skill/Tool
//! security invariants are the same code in local and cloud mode, and it calls
//! the provider adapters in-process because there is no boundary to cross.

pub mod admission;
pub mod client;
mod durable_file;
pub mod embedded;
mod event_archive;
pub mod grpc;
pub mod ipc;
pub mod retention;
mod stdio_mcp;

use agent_kernel::ToolPlan;
use agent_model_gateway::mcp::{
    McpCallLifecycle, McpFederationClient as DirectMcpFederationClient, McpFederationError,
    McpRoundTripContinuation as DirectMcpRoundTripContinuation, McpServerRef as DirectMcpServerRef,
    McpToolCallOutcome as DirectMcpToolCallOutcome,
};
use agent_model_gateway::{
    AnthropicMessagesAdapter, AnthropicMessagesConfig, Capability, DataClass, ModelCandidate,
    OpenAiCompatibleAdapter, OpenAiCompatibleConfig, OpenAiResponsesAdapter, OpenAiResponsesConfig,
    ProviderAdapter, ProviderCredential, ProviderExecutionError, ProviderPricing, ProviderProtocol,
    RoutingConstraints, decode_model_invocation, rank_candidates,
};
use agent_protocol::{
    AgentLineage, ApprovalMode, ContentPart as ProtocolContentPart, EventEnvelope, HistoryImport,
    HistoryImportSource, HistoryRepairReport, McpClientCapability, McpInputContinuation,
    McpInputRequired, McpInputResolutionCommand, McpInputResponse, McpPromptPage, McpPromptResult,
    McpProtocolRevision, McpResourcePage, McpResourceReadResult, McpResourceTemplatePage,
    Message as ProtocolMessage, ModelErrorKind, ModelStreamEvent, RUN_EXECUTION_SCHEMA_VERSION,
    Role as ProtocolRole, RunBudget, RunExecutionCommand, RunStatus,
    RuntimeExecutionPolicySnapshot, RuntimeInvocationContext, SandboxClass, SessionBranchSnapshot,
    SessionConversationTurn, SkillSnapshot, SubagentBudgetUsage, SubagentConversationTurn,
    SubagentResultDelivery, SubagentResultOutcome, SubagentResultSource, SubagentRole,
    SubagentSpawnMode, TOOL_APPROVAL_DECISION_SCHEMA_VERSION, ToolApprovalDecision,
    ToolApprovalDecisionCommand, ToolApprovalRequest, ToolDescriptor, ToolEffect,
    ToolReconciliationCommand, ToolReconciliationDecision,
};
use agent_runtime_worker::{
    DiscoveredCatalog, DiscoveredTool, FederationIdentity, McpCallContext, McpDiscoveryCompletion,
    McpDiscoveryCoordinator, McpFederationBackend, McpFederationClient, McpGatewayClientError,
    McpServerDiscoveryStatus, McpToolRoundOutcome, SubagentMessageStatus, WorkerProcessor,
    WorkerRecoveryAction, WorkerToolDefinition,
};
use agent_tool_runtime::{
    PROCESS_ATTACH_TOOL, PROCESS_CLOSE_TOOL, PROCESS_INTERRUPT_TOOL, PROCESS_POLL_TOOL,
    PROCESS_RESIZE_TOOL, PROCESS_START_TOOL, PROCESS_WAIT_TOOL, PROCESS_WRITE_TOOL,
    PersistentProcessSessionManager, ProcessSessionGovernance, ProcessSessionPtySupervisorConfig,
    ProcessSessionToolExecutor, ProcessSessionToolOperation, ToolExecutionContext,
    ToolExecutionError, ToolExecutionResult, ToolExecutor, ToolProgressReporter,
    TrustedNativeExecutor, TrustedNativeToolDefinition, WorkspaceAccess,
};
use base64::Engine;
use chrono::{Duration as ChronoDuration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex as StdMutex, OnceLock, Weak};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use stdio_mcp::StdioMcpClient;

/// The trusted read-only workspace Tool, the only Tool a local host installs.
pub const WORKSPACE_READ_TOOL: &str = "workspace.read_text";
/// Scope the workspace Tool requires; a local Run that does not delegate it
/// cannot see the Tool, exactly as in cloud mode.
pub const WORKSPACE_READ_SCOPE: &str = "tool:workspace.read";
/// Write-capable counterpart. Contained by Seatbelt to the Workspace (ADR-0036)
/// and approval gated, because it changes the user's files.
pub const WORKSPACE_WRITE_TOOL: &str = "workspace.write_text";
pub const WORKSPACE_WRITE_SCOPE: &str = "tool:workspace.write";
/// Shell, same Tool and same containment as the cloud Worker installs. A local
/// host that offered fewer Tools than the cloud one would make the desktop
/// client a weaker product for no security reason -- the boundary is the
/// container, and it is the same container.
pub const SHELL_TOOL: &str = "shell.exec";
pub const SHELL_SCOPE: &str = "tool:shell.exec";
/// One scope gates the complete durable process-session lifecycle. Individual
/// operations still have their own effect and approval binding.
pub const PROCESS_SESSION_SCOPE: &str = "tool:process.session";

pub(crate) const LOCAL_STORE_VERSION: u32 = 1;
pub(crate) const LOCAL_EVENT_LOG_LINE_MAX_BYTES: usize = 256 * 1024;

/// A local host has exactly one configured provider, so its model policy
/// identity is a fixed local constant. It must be stable across restarts:
/// recovery compares it against the identity the Checkpoint bound, and a fresh
/// value per process would make every local Run unresumable.
const LOCAL_MODEL_POLICY_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0001);

/// Fixed identities for the single local tenancy. The nil UUID is the "absent"
/// sentinel, so using it as a real identity both reads as missing data and is
/// rejected outright by contracts that require a complete identity.
pub const LOCAL_TENANT_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0010);
const LOCAL_WORKSPACE_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0011);
const LOCAL_AGENT_VERSION_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0012);
const LOCAL_APPLICATION_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0013);
const LOCAL_SKILL_VERSION_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0014);
const LOCAL_SKILL_KEY_ID: &str = "local-runtime-host-key";
const LOCAL_SKILL_SIGNING_KEY: [u8; 32] = [0x52; 32];

/// Explicit compatibility profile for the single-user CLI/desktop path.
/// Production and embedded callers use `start_for_invocation`; keeping these
/// values in one constructor prevents them from leaking into that path.
#[must_use]
pub const fn local_invocation_context() -> RuntimeInvocationContext {
    RuntimeInvocationContext {
        schema_version: agent_protocol::RUNTIME_INVOCATION_SCHEMA_VERSION,
        tenant_id: LOCAL_TENANT_ID,
        application_id: LOCAL_APPLICATION_ID,
        workload_identity_id: Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0015),
        workspace_id: LOCAL_WORKSPACE_ID,
        agent_version_id: LOCAL_AGENT_VERSION_ID,
        model_policy_id: LOCAL_MODEL_POLICY_ID,
    }
}

fn local_skill_snapshot(
    invocation: RuntimeInvocationContext,
    has_trusted_tool: bool,
    has_process_session: bool,
    mcp_servers: &[LocalMcpServerConfig],
) -> SkillSnapshot {
    let mut tool_names = BTreeSet::new();
    if has_trusted_tool {
        tool_names.extend([
            SHELL_TOOL.to_owned(),
            WORKSPACE_READ_TOOL.to_owned(),
            WORKSPACE_WRITE_TOOL.to_owned(),
        ]);
    }
    if has_process_session {
        tool_names.extend([
            PROCESS_START_TOOL.to_owned(),
            PROCESS_WRITE_TOOL.to_owned(),
            PROCESS_RESIZE_TOOL.to_owned(),
            PROCESS_POLL_TOOL.to_owned(),
            PROCESS_ATTACH_TOOL.to_owned(),
            PROCESS_WAIT_TOOL.to_owned(),
            PROCESS_INTERRUPT_TOOL.to_owned(),
            PROCESS_CLOSE_TOOL.to_owned(),
        ]);
    }
    for server in mcp_servers {
        tool_names.extend(
            server
                .tool_names
                .iter()
                .map(|tool| format!("mcp:{}/{tool}", server.name)),
        );
    }
    let mut snapshot = SkillSnapshot {
        schema_version: 1,
        application_id: invocation.application_id,
        skill_version_id: LOCAL_SKILL_VERSION_ID,
        name: "local-runtime-tools".into(),
        semantic_version: "1.0.0".into(),
        description: "Tools installed by the standalone Rust runtime host".into(),
        instructions: "Use only the explicitly declared local tools when they are needed.".into(),
        tool_names: tool_names.into_iter().collect(),
        supported_platforms: vec![
            "darwin-arm64".into(),
            "linux-arm64".into(),
            "linux-x86_64".into(),
        ],
        min_runtime_version: env!("CARGO_PKG_VERSION").into(),
        artifact_digest: String::new(),
        signing_key_id: LOCAL_SKILL_KEY_ID.into(),
        signature: String::new(),
    };
    snapshot.artifact_digest = snapshot.expected_artifact_digest(invocation.tenant_id);
    let key = SigningKey::from_bytes(&LOCAL_SKILL_SIGNING_KEY);
    let signature =
        key.sign(format!("agent-runtime-skill-v1.{}", snapshot.artifact_digest).as_bytes());
    snapshot.signature =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes());
    snapshot
}

#[cfg(test)]
mod process_start_failure_agent_loop_tests {
    use super::*;
    use std::io::Write as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const PRIVATE_START_REASON: &str = "private-start-reason-must-not-leak";
    const PRIVATE_AMBIGUOUS_REASON: &str = "private-side-effect-failure-must-not-leak";

    #[test]
    fn standalone_runtime_rejects_a_gateway_owned_oauth_handle() {
        let server = agent_protocol::McpServerSnapshot {
            server_id: Uuid::now_v7(),
            name: "oauth".into(),
            endpoint: "https://mcp.example.test/rpc".into(),
            credential_envelope_base64: String::new(),
            oauth_credential_id: Some(Uuid::now_v7()),
            required: true,
            tool_effect_overrides: BTreeMap::new(),
            protocol_revision: McpProtocolRevision::V2025_06_18,
            client_capabilities: BTreeSet::new(),
        };

        assert!(
            local_direct_server(&server, &server.endpoint).is_err(),
            "standalone mode must not collapse the Gateway credential domain"
        );
    }

    #[derive(Clone)]
    struct StartFailureExecutor {
        implementation_digest: String,
        session_id: Uuid,
    }

    impl ToolExecutor for StartFailureExecutor {
        fn implementation_digest(&self) -> &str {
            &self.implementation_digest
        }

        fn execute(
            &self,
            _request: agent_protocol::ToolExecutionRequest,
            _context: ToolExecutionContext,
        ) -> Pin<
            Box<dyn Future<Output = Result<ToolExecutionResult, ToolExecutionError>> + Send + '_>,
        > {
            Box::pin(std::future::ready(Err(
                ToolExecutionError::ProcessSessionStartFailed {
                    session_id: self.session_id,
                    reason: PRIVATE_START_REASON.into(),
                },
            )))
        }
    }

    #[derive(Clone)]
    struct SideEffectThenFailureExecutor {
        implementation_digest: String,
        marker: PathBuf,
    }

    impl ToolExecutor for SideEffectThenFailureExecutor {
        fn implementation_digest(&self) -> &str {
            &self.implementation_digest
        }

        fn execute(
            &self,
            _request: agent_protocol::ToolExecutionRequest,
            _context: ToolExecutionContext,
        ) -> Pin<
            Box<dyn Future<Output = Result<ToolExecutionResult, ToolExecutionError>> + Send + '_>,
        > {
            Box::pin(async move {
                let mut marker = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.marker)
                    .unwrap();
                marker.write_all(b"effect-applied\n").unwrap();
                marker.flush().unwrap();
                Err(ToolExecutionError::Engine(PRIVATE_AMBIGUOUS_REASON.into()))
            })
        }
    }

    async fn read_request(socket: &mut TcpStream) -> serde_json::Value {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        let (header_end, content_length) = loop {
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "provider request ended before headers");
            request.extend_from_slice(&chunk[..read]);
            if let Some(header_end) = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|position| position + 4)
            {
                let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                    })
                    .unwrap();
                break (header_end, content_length);
            }
        };
        while request.len() < header_end + content_length {
            let read = socket.read(&mut chunk).await.unwrap();
            assert!(read > 0, "provider request ended before body");
            request.extend_from_slice(&chunk[..read]);
        }
        serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap()
    }

    async fn respond(socket: &mut TcpStream, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
    }

    fn tool_turn() -> String {
        let delta = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "failed_process_start",
                        "type": "function",
                        "function": {"name": PROCESS_START_TOOL, "arguments": "{}"}
                    }]
                }
            }]
        });
        format!(
            "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        )
    }

    fn ambiguous_write_turn() -> String {
        let delta = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_ambiguous_write",
                        "type": "function",
                        "function": {
                            "name": "workspace.write_text",
                            "arguments": "{\"path\":\"effect.txt\",\"text\":\"once\"}"
                        }
                    }]
                }
            }]
        });
        format!(
            "data: {delta}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        )
    }

    fn text_turn(text: &str) -> String {
        format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n\
             data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
             data: [DONE]\n\n"
        )
    }

    fn latest_tool_result(request: &serde_json::Value) -> serde_json::Value {
        let content = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|message| message["role"] == "tool")
            .and_then(|message| message["content"].as_str())
            .expect("follow-up request contains the failed Tool result");
        serde_json::from_str(content).unwrap()
    }

    async fn spawn_provider(
        session_id: Uuid,
    ) -> (String, tokio::task::JoinHandle<serde_json::Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut first).await;
            respond(&mut first, &tool_turn()).await;

            let (mut second, _) = listener.accept().await.unwrap();
            let request = read_request(&mut second).await;
            let result = latest_tool_result(&request);
            assert_eq!(result["error"]["code"], "process_session_start_failed");
            assert_eq!(result["error"]["session_id"], session_id.to_string());
            assert_eq!(
                result["error"]["message"],
                "persistent process session could not be started"
            );
            assert!(
                !result.to_string().contains(PRIVATE_START_REASON),
                "the model-visible result leaked the private OS reason"
            );
            respond(&mut second, &text_turn("start failure handled")).await;
            result
        });
        (
            format!("http://127.0.0.1:{port}/v1/chat/completions"),
            handle,
        )
    }

    async fn spawn_ambiguous_provider() -> (String, tokio::task::JoinHandle<serde_json::Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut first).await;
            respond(&mut first, &ambiguous_write_turn()).await;

            let (mut second, _) = listener.accept().await.unwrap();
            let request = read_request(&mut second).await;
            let result = latest_tool_result(&request);
            assert_eq!(
                result["reconciliation"]["decision"], "applied",
                "continuation did not receive operator reconciliation"
            );
            assert_eq!(result["result"]["written"], true);
            respond(&mut second, &text_turn("continued without replay")).await;
            result
        });
        (
            format!("http://127.0.0.1:{port}/v1/chat/completions"),
            handle,
        )
    }

    fn executable_script(root: &Path) -> PathBuf {
        let executable = root.join("process-session");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        executable
    }

    #[tokio::test]
    async fn standalone_agent_loop_returns_typed_start_failure_without_leaking_the_os_reason() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let trusted = tempfile::tempdir().unwrap();
        let executable = executable_script(trusted.path());
        let session_id = Uuid::now_v7();
        let (endpoint, provider) = spawn_provider(session_id).await;
        let mut host = LocalRuntimeHost::start(LocalRuntimeConfig {
            state_root: state.path().to_path_buf(),
            workspace_root: workspace.path().canonicalize().unwrap(),
            agent_instructions: "Handle the process start result.".into(),
            delegated_scopes: BTreeSet::from([PROCESS_SESSION_SCOPE.to_owned()]),
            subagent_roles: Vec::new(),
            model_routing: LocalModelRoutingConfig::single_openai_compatible(
                endpoint,
                "loopback-model",
                "loopback-key",
            ),
            mcp_servers: Vec::new(),
            mcp_lifecycle: LocalMcpLifecycleConfig::default(),
            trusted_workspace_tool: None,
            process_session: Some(LocalProcessSessionConfig {
                executable,
                fixed_args: Vec::new(),
                max_output_chunk_bytes: 16 * 1024,
                governance: ProcessSessionGovernance::default(),
                pty_supervisor: None,
            }),
            consent: LocalToolConsent::AllowOnce,
            budget: RunBudget {
                max_tokens: 8_192,
                max_cost_cents: 100,
                max_duration_seconds: 600,
            },
            runtime_policy: RuntimeExecutionPolicySnapshot::default(),
        })
        .unwrap();
        let implementation_digest = host.executors[PROCESS_START_TOOL]
            .implementation_digest()
            .to_owned();
        host.executors.insert(
            PROCESS_START_TOOL.to_owned(),
            Arc::new(StartFailureExecutor {
                implementation_digest,
                session_id,
            }),
        );

        let outcome = host
            .execute("Start the process and handle a deterministic start failure.")
            .await
            .expect("a typed pre-start failure must return to the Agent Loop");
        assert_eq!(outcome.status, RunStatus::Succeeded);
        assert_eq!(outcome.output, "start failure handled");
        let provider_result = provider.await.unwrap();
        let events = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0).unwrap();
        let tool_result = events
            .iter()
            .find(|event| event.event_type == "tool.result")
            .expect("typed start failure is durable as a Tool result");
        assert_eq!(tool_result.payload["content"], provider_result);
        assert_eq!(tool_result.payload["is_error"], true);
    }

    #[tokio::test]
    async fn standalone_agent_loop_makes_a_live_ambiguous_failure_reconcilable_without_replay() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let trusted = tempfile::tempdir().unwrap();
        let executable = executable_script(trusted.path());
        let marker = workspace.path().join("side-effect-marker");
        let (endpoint, provider) = spawn_ambiguous_provider().await;
        let mut host = LocalRuntimeHost::start(LocalRuntimeConfig {
            state_root: state.path().to_path_buf(),
            workspace_root: workspace.path().canonicalize().unwrap(),
            agent_instructions: "Handle the write Tool result.".into(),
            delegated_scopes: BTreeSet::from(["tool:workspace.write".to_owned()]),
            subagent_roles: Vec::new(),
            model_routing: LocalModelRoutingConfig::single_openai_compatible(
                endpoint,
                "loopback-model",
                "loopback-key",
            ),
            mcp_servers: Vec::new(),
            mcp_lifecycle: LocalMcpLifecycleConfig::default(),
            trusted_workspace_tool: Some(executable),
            process_session: None,
            consent: LocalToolConsent::AllowOnce,
            budget: RunBudget {
                max_tokens: 8_192,
                max_cost_cents: 100,
                max_duration_seconds: 600,
            },
            runtime_policy: RuntimeExecutionPolicySnapshot::default(),
        })
        .unwrap();
        let implementation_digest = host.executors["workspace.write_text"]
            .implementation_digest()
            .to_owned();
        host.executors.insert(
            "workspace.write_text".to_owned(),
            Arc::new(SideEffectThenFailureExecutor {
                implementation_digest,
                marker: marker.clone(),
            }),
        );

        let outcome = host
            .execute("Write the side effect once.")
            .await
            .expect("an ambiguous execution error must become a durable Run outcome");
        assert_eq!(outcome.status, RunStatus::Indeterminate);
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "effect-applied\n"
        );
        let events = LocalRuntimeHost::replay_events(state.path(), outcome.run_id, 0).unwrap();
        let terminal = events
            .iter()
            .find(|event| event.event_type == "run.indeterminate")
            .expect("live executor failure produced an indeterminate terminal");
        assert_eq!(terminal.payload["tool_call_id"], "call_ambiguous_write");
        assert_eq!(terminal.payload["effect"], "non_idempotent");
        assert_eq!(terminal.payload["replay_safe"], false);
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains(PRIVATE_AMBIGUOUS_REASON),
            "private executor diagnostics leaked into durable events"
        );
        let checkpoint = LocalRuntimeHost::load_checkpoint(&outcome.checkpoint_path).unwrap();
        assert_eq!(checkpoint.status, RunStatus::Indeterminate);

        let reconciliation = ToolReconciliationCommand {
            schema_version: agent_protocol::TOOL_RECONCILIATION_SCHEMA_VERSION,
            reconciliation_id: Uuid::now_v7(),
            version: 1,
            tenant_id: LOCAL_TENANT_ID,
            source_run_id: outcome.run_id,
            source_terminal_event_id: terminal.event_id,
            tool_call_id: "call_ambiguous_write".into(),
            binding_digest: terminal.payload["binding_digest"]
                .as_str()
                .unwrap()
                .to_owned(),
            operator_id: "operator@example.test".into(),
            decision: ToolReconciliationDecision::Applied {
                content: serde_json::json!({"written": true}),
                is_error: false,
            },
            continuation_input: Some("Continue from the confirmed write.".into()),
            issued_at: Utc::now(),
        };
        let reconciled = host
            .reconcile_tool_outcome(reconciliation)
            .await
            .expect("operator evidence must start a separate continuation Run");
        let continuation = reconciled.continuation.expect("continuation Run");
        assert_eq!(continuation.status, RunStatus::Succeeded);
        assert_eq!(continuation.output, "continued without replay");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "effect-applied\n"
        );
        let provider_result = provider.await.unwrap();
        assert_eq!(provider_result["reconciliation"]["decision"], "applied");
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LocalRuntimeError {
    #[error("local runtime configuration is invalid: {0}")]
    Configuration(String),
    #[error("local state root is not usable: {0}")]
    StateRoot(String),
    /// Distinct from StateRoot on purpose: a client that sees this should
    /// connect to the running host, not report a broken installation.
    #[error("another runtime host is already serving this state root at {0}")]
    AlreadyRunning(String),
    #[error("local execution was refused: {0}")]
    Execution(String),
    #[error("no model provider can serve this Run: {0}")]
    ProviderSelection(String),
    #[error("model provider call failed: {0}")]
    Provider(String),
    #[error("trusted tool execution failed: {0}")]
    ToolExecution(String),
    #[error("local checkpoint is unusable: {0}")]
    Checkpoint(String),
}

/// The operator's answer to a parked approval.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalApprovalDecision {
    AllowOnce,
    Deny,
}

/// How the local operator's consent reaches an approval-gated Tool. The gate is
/// never removed; `AllowOnce` only supplies the decision the cloud console
/// would supply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalToolConsent {
    Ask,
    AllowOnce,
}

#[derive(Clone)]
pub struct LocalProviderConfig {
    pub id: String,
    pub protocol: ProviderProtocol,
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub region: String,
    pub accepted_data_classes: BTreeSet<DataClass>,
    pub capabilities: BTreeSet<Capability>,
    pub healthy: bool,
    pub latency_ms: u64,
    pub cost_per_million_tokens_micros: u64,
    pub response_timeout_ms: u64,
    pub stream_idle_timeout_ms: u64,
}

impl std::fmt::Debug for LocalProviderConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalProviderConfig")
            .field("id", &self.id)
            .field("protocol", &self.protocol)
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("api_key", &"[REDACTED]")
            .field("region", &self.region)
            .field("accepted_data_classes", &self.accepted_data_classes)
            .field("capabilities", &self.capabilities)
            .field("healthy", &self.healthy)
            .field("latency_ms", &self.latency_ms)
            .field(
                "cost_per_million_tokens_micros",
                &self.cost_per_million_tokens_micros,
            )
            .field("response_timeout_ms", &self.response_timeout_ms)
            .field("stream_idle_timeout_ms", &self.stream_idle_timeout_ms)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct LocalModelRoutingConfig {
    pub candidates: Vec<LocalProviderConfig>,
    pub allowed_regions: BTreeSet<String>,
    pub data_class: DataClass,
    pub max_cost_per_million_tokens_micros: u64,
    pub health_policy: LocalProviderHealthPolicy,
}

/// Process-independent Provider lifecycle bounds for the standalone Host.
/// These values are part of the immutable local model-policy digest; changing
/// them cannot silently alter an already checkpointed Run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalProviderHealthPolicy {
    /// Total attempts against one candidate before advancing, including the
    /// first request. Only zero-event retryable failures may consume more than
    /// one attempt.
    pub max_same_provider_attempts: u8,
    pub initial_retry_backoff_ms: u64,
    pub max_retry_backoff_ms: u64,
    pub consecutive_failure_threshold: u8,
    pub cooldown_ms: u64,
    pub max_retry_after_ms: u64,
    pub half_open_probe_lease_ms: u64,
}

impl Default for LocalProviderHealthPolicy {
    fn default() -> Self {
        Self {
            max_same_provider_attempts: 1,
            initial_retry_backoff_ms: 100,
            max_retry_backoff_ms: 2_000,
            consecutive_failure_threshold: 2,
            cooldown_ms: 30_000,
            max_retry_after_ms: 60_000,
            half_open_probe_lease_ms: 120_000,
        }
    }
}

impl LocalProviderHealthPolicy {
    fn is_bounded_and_safe(&self) -> bool {
        (1..=4).contains(&self.max_same_provider_attempts)
            && self.initial_retry_backoff_ms <= 5_000
            && self.max_retry_backoff_ms >= self.initial_retry_backoff_ms
            && self.max_retry_backoff_ms <= 60_000
            && (1..=8).contains(&self.consecutive_failure_threshold)
            && (1..=3_600_000).contains(&self.cooldown_ms)
            && (1..=3_600_000).contains(&self.max_retry_after_ms)
            && (1..=3_600_000).contains(&self.half_open_probe_lease_ms)
    }
}

impl LocalModelRoutingConfig {
    #[must_use]
    pub fn single_openai_compatible(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            candidates: vec![LocalProviderConfig {
                id: "embedded-local".into(),
                protocol: ProviderProtocol::OpenAiCompatible,
                endpoint: endpoint.into(),
                model: model.into(),
                api_key: api_key.into(),
                region: "local".into(),
                accepted_data_classes: BTreeSet::from([
                    DataClass::Public,
                    DataClass::Internal,
                    DataClass::Confidential,
                    DataClass::Restricted,
                ]),
                capabilities: BTreeSet::from([
                    Capability::Text,
                    Capability::Vision,
                    Capability::ToolUse,
                ]),
                healthy: true,
                latency_ms: 0,
                cost_per_million_tokens_micros: 0,
                response_timeout_ms: 120_000,
                stream_idle_timeout_ms: 60_000,
            }],
            allowed_regions: BTreeSet::from(["local".into()]),
            data_class: DataClass::Internal,
            max_cost_per_million_tokens_micros: u64::MAX,
            health_policy: LocalProviderHealthPolicy::default(),
        }
    }
}

/// Transport owned by one explicitly configured local MCP server.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalMcpTransportConfig {
    StreamableHttp {
        endpoint: String,
    },
    /// Stateless MCP 2026-07-28.  Enabling elicitation is an explicit local
    /// authority grant; it is never inferred from the peer's response.
    StreamableHttp2026 {
        endpoint: String,
        #[serde(default)]
        elicitation: bool,
    },
    Stdio {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    /// Stateful MCP 2025-03-26 over a persistent local process. This remains
    /// explicit rather than silently accepting a peer-selected downgrade, so
    /// the exact wire revision is frozen into Run and Checkpoint identity.
    #[serde(rename = "stdio_2025_03_26")]
    StdioV20250326 {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    /// Stateless MCP 2026-07-28 over a persistent local process. Elicitation
    /// remains an explicit authority grant, identical to the HTTP transport.
    Stdio2026 {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        elicitation: bool,
    },
}

impl LocalMcpTransportConfig {
    fn binding_endpoint(&self) -> String {
        match self {
            Self::StreamableHttp { endpoint } | Self::StreamableHttp2026 { endpoint, .. } => {
                endpoint.clone()
            }
            Self::Stdio { .. } | Self::StdioV20250326 { .. } | Self::Stdio2026 { .. } => {
                let canonical =
                    serde_json::to_vec(self).expect("local MCP transport config is serializable");
                format!("stdio+sha256://{}", hex::encode(Sha256::digest(canonical)))
            }
        }
    }

    fn protocol_revision(&self) -> McpProtocolRevision {
        match self {
            Self::StreamableHttp2026 { .. } | Self::Stdio2026 { .. } => {
                McpProtocolRevision::V2026_07_28
            }
            Self::StdioV20250326 { .. } => McpProtocolRevision::V2025_03_26,
            Self::StreamableHttp { .. } | Self::Stdio { .. } => McpProtocolRevision::V2025_06_18,
        }
    }

    fn client_capabilities(&self) -> BTreeSet<McpClientCapability> {
        match self {
            Self::StreamableHttp2026 {
                elicitation: true, ..
            }
            | Self::Stdio2026 {
                elicitation: true, ..
            } => BTreeSet::from([McpClientCapability::Elicitation]),
            _ => BTreeSet::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LocalStdioMcpConfig {
    command: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
    protocol_revision: McpProtocolRevision,
    client_capabilities: BTreeSet<McpClientCapability>,
}

/// One credential-free MCP endpoint explicitly trusted by the local operator.
/// `tool_names` is the signed Skill allowlist; discovery may narrow it but can
/// never add a Tool the local configuration did not name.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalMcpServerConfig {
    pub server_id: Uuid,
    pub name: String,
    pub transport: LocalMcpTransportConfig,
    pub tool_names: BTreeSet<String>,
    /// Operator-owned effect declarations keyed by server-local Tool name.
    /// Missing entries remain `Unknown`; remote MCP annotations are ignored.
    #[serde(default)]
    pub tool_effect_overrides: BTreeMap<String, ToolEffect>,
    /// Required servers are part of the accepted Agent capability contract;
    /// optional failures remain visible in the Run outcome but do not block it.
    #[serde(default)]
    pub required: bool,
}

/// Process-local MCP lifecycle optimization. The frozen catalog digest remains
/// the Run authority; this cache may avoid a safe `tools/list`, but can never
/// authorize or replay a Tool call.
#[derive(Clone, Debug)]
pub struct LocalMcpLifecycleConfig {
    pub catalog_ttl: Duration,
    pub session_idle_ttl: Duration,
    pub sweep_interval: Duration,
    pub max_sessions: usize,
}

impl Default for LocalMcpLifecycleConfig {
    fn default() -> Self {
        Self {
            catalog_ttl: Duration::from_secs(30 * 60),
            session_idle_ttl: Duration::from_secs(10 * 60),
            sweep_interval: Duration::from_secs(60),
            max_sessions: 32,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LocalMcpLifecycleSnapshot {
    pub catalog_cache_hits: u64,
    pub catalog_cache_misses: u64,
    pub failed_session_retirements: u64,
    pub live_sessions: usize,
    pub active_leases: usize,
    pub cached_catalogs: usize,
    pub idle_evictions: u64,
    pub lru_evictions: u64,
}

#[derive(Clone)]
struct LocalMcpBackend {
    http: DirectMcpFederationClient,
    transports: Arc<HashMap<Uuid, LocalMcpTransportConfig>>,
    stdio: StdioMcpClient,
}

impl LocalMcpBackend {
    fn transport(
        &self,
        server: &agent_protocol::McpServerSnapshot,
    ) -> Result<&LocalMcpTransportConfig, McpGatewayClientError> {
        self.transports.get(&server.server_id).ok_or_else(|| {
            McpGatewayClientError::InvalidResponse(
                "standalone MCP server is not in the local transport registry".into(),
            )
        })
    }
}

impl McpFederationBackend for LocalMcpBackend {
    fn list_tools<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a agent_protocol::McpServerSnapshot,
        _workload_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<DiscoveredCatalog, McpGatewayClientError>> + Send + 'a>>
    {
        Box::pin(async move {
            let catalog = match self.transport(server)? {
                LocalMcpTransportConfig::StreamableHttp { endpoint }
                | LocalMcpTransportConfig::StreamableHttp2026 { endpoint, .. } => {
                    let direct = local_direct_server(server, endpoint)?;
                    self.http
                        .list_tools(identity.tenant_id, &direct)
                        .await
                        .map_err(local_mcp_error)?
                }
                LocalMcpTransportConfig::Stdio { .. }
                | LocalMcpTransportConfig::StdioV20250326 { .. }
                | LocalMcpTransportConfig::Stdio2026 { .. } => self
                    .stdio
                    .list_tools(server.server_id, &server.name)
                    .await
                    .map_err(local_mcp_error)?,
            };
            let tools = catalog
                .tools
                .into_iter()
                .map(|tool| {
                    let input_schema =
                        serde_json::from_str(&tool.input_schema_json).map_err(|error| {
                            McpGatewayClientError::InvalidResponse(error.to_string())
                        })?;
                    Ok(DiscoveredTool {
                        qualified_name: tool.qualified_name,
                        description: tool.description,
                        input_schema,
                    })
                })
                .collect::<Result<Vec<_>, McpGatewayClientError>>()?;
            Ok(DiscoveredCatalog {
                tools,
                capabilities: catalog.capabilities,
                digest: catalog.digest,
            })
        })
    }

    fn list_resources<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a agent_protocol::McpServerSnapshot,
        frozen_catalog_digest: &'a str,
        cursor: Option<&'a str>,
        _workload_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<McpResourcePage, McpGatewayClientError>> + Send + 'a>>
    {
        Box::pin(async move {
            match self.transport(server)? {
                LocalMcpTransportConfig::StreamableHttp { endpoint }
                | LocalMcpTransportConfig::StreamableHttp2026 { endpoint, .. } => {
                    let direct = local_direct_server(server, endpoint)?;
                    self.http
                        .list_resources(identity.tenant_id, &direct, frozen_catalog_digest, cursor)
                        .await
                        .map_err(local_mcp_error)
                }
                LocalMcpTransportConfig::Stdio { .. }
                | LocalMcpTransportConfig::StdioV20250326 { .. }
                | LocalMcpTransportConfig::Stdio2026 { .. } => self
                    .stdio
                    .list_resources(
                        server.server_id,
                        &server.name,
                        frozen_catalog_digest,
                        cursor,
                    )
                    .await
                    .map_err(local_mcp_error),
            }
        })
    }

    fn read_resource<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a agent_protocol::McpServerSnapshot,
        frozen_catalog_digest: &'a str,
        uri: &'a str,
        _workload_token: &'a str,
    ) -> Pin<
        Box<dyn Future<Output = Result<McpResourceReadResult, McpGatewayClientError>> + Send + 'a>,
    > {
        Box::pin(async move {
            match self.transport(server)? {
                LocalMcpTransportConfig::StreamableHttp { endpoint }
                | LocalMcpTransportConfig::StreamableHttp2026 { endpoint, .. } => {
                    let direct = local_direct_server(server, endpoint)?;
                    self.http
                        .read_resource(identity.tenant_id, &direct, frozen_catalog_digest, uri)
                        .await
                        .map_err(local_mcp_error)
                }
                LocalMcpTransportConfig::Stdio { .. }
                | LocalMcpTransportConfig::StdioV20250326 { .. }
                | LocalMcpTransportConfig::Stdio2026 { .. } => self
                    .stdio
                    .read_resource(server.server_id, &server.name, frozen_catalog_digest, uri)
                    .await
                    .map_err(local_mcp_error),
            }
        })
    }

    fn list_resource_templates<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a agent_protocol::McpServerSnapshot,
        frozen_catalog_digest: &'a str,
        cursor: Option<&'a str>,
        _workload_token: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<McpResourceTemplatePage, McpGatewayClientError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            match self.transport(server)? {
                LocalMcpTransportConfig::StreamableHttp { endpoint }
                | LocalMcpTransportConfig::StreamableHttp2026 { endpoint, .. } => {
                    let direct = local_direct_server(server, endpoint)?;
                    self.http
                        .list_resource_templates(
                            identity.tenant_id,
                            &direct,
                            frozen_catalog_digest,
                            cursor,
                        )
                        .await
                        .map_err(local_mcp_error)
                }
                LocalMcpTransportConfig::Stdio { .. }
                | LocalMcpTransportConfig::StdioV20250326 { .. }
                | LocalMcpTransportConfig::Stdio2026 { .. } => self
                    .stdio
                    .list_resource_templates(
                        server.server_id,
                        &server.name,
                        frozen_catalog_digest,
                        cursor,
                    )
                    .await
                    .map_err(local_mcp_error),
            }
        })
    }

    fn list_prompts<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a agent_protocol::McpServerSnapshot,
        frozen_catalog_digest: &'a str,
        cursor: Option<&'a str>,
        _workload_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<McpPromptPage, McpGatewayClientError>> + Send + 'a>>
    {
        Box::pin(async move {
            match self.transport(server)? {
                LocalMcpTransportConfig::StreamableHttp { endpoint }
                | LocalMcpTransportConfig::StreamableHttp2026 { endpoint, .. } => {
                    let direct = local_direct_server(server, endpoint)?;
                    self.http
                        .list_prompts(identity.tenant_id, &direct, frozen_catalog_digest, cursor)
                        .await
                        .map_err(local_mcp_error)
                }
                LocalMcpTransportConfig::Stdio { .. }
                | LocalMcpTransportConfig::StdioV20250326 { .. }
                | LocalMcpTransportConfig::Stdio2026 { .. } => self
                    .stdio
                    .list_prompts(
                        server.server_id,
                        &server.name,
                        frozen_catalog_digest,
                        cursor,
                    )
                    .await
                    .map_err(local_mcp_error),
            }
        })
    }

    fn get_prompt<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a agent_protocol::McpServerSnapshot,
        frozen_catalog_digest: &'a str,
        name: &'a str,
        arguments: Option<&'a serde_json::Value>,
        _workload_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<McpPromptResult, McpGatewayClientError>> + Send + 'a>>
    {
        Box::pin(async move {
            match self.transport(server)? {
                LocalMcpTransportConfig::StreamableHttp { endpoint }
                | LocalMcpTransportConfig::StreamableHttp2026 { endpoint, .. } => {
                    let direct = local_direct_server(server, endpoint)?;
                    self.http
                        .get_prompt(
                            identity.tenant_id,
                            &direct,
                            frozen_catalog_digest,
                            name,
                            arguments,
                        )
                        .await
                        .map_err(local_mcp_error)
                }
                LocalMcpTransportConfig::Stdio { .. }
                | LocalMcpTransportConfig::StdioV20250326 { .. }
                | LocalMcpTransportConfig::Stdio2026 { .. } => self
                    .stdio
                    .get_prompt(
                        server.server_id,
                        &server.name,
                        frozen_catalog_digest,
                        name,
                        arguments,
                    )
                    .await
                    .map_err(local_mcp_error),
            }
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the federation trust boundary keeps identity, authority and lifecycle inputs explicit"
    )]
    fn call_tool<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a agent_protocol::McpServerSnapshot,
        qualified_name: &'a str,
        arguments: &'a serde_json::Value,
        frozen_catalog_digest: &'a str,
        _workload_token: &'a str,
        context: &'a McpCallContext,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(serde_json::Value, bool), McpGatewayClientError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let result = match self.transport(server)? {
                LocalMcpTransportConfig::StreamableHttp { endpoint } => {
                    let direct = local_direct_server(server, endpoint)?;
                    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(32);
                    let lifecycle = McpCallLifecycle {
                        cancellation: context.cancellation.clone(),
                        progress: progress_tx,
                        progress_token: context.progress_token.clone(),
                        cancellation_reason: context.cancellation_reason.clone(),
                    };
                    let arguments_json = arguments.to_string();
                    let call = self.http.call_tool_with_lifecycle(
                        identity.tenant_id,
                        &direct,
                        qualified_name,
                        &arguments_json,
                        frozen_catalog_digest,
                        &lifecycle,
                    );
                    tokio::pin!(call);
                    loop {
                        tokio::select! {
                            biased;
                            result = &mut call => break result.map_err(local_mcp_error)?,
                            update = progress_rx.recv() => {
                                let Some(update) = update else {
                                    continue;
                                };
                                let _ = context.progress.try_send(
                                    agent_runtime_worker::McpProgressNotification {
                                        progress: update.progress,
                                        total: update.total,
                                        message: update.message,
                                    }
                                );
                            }
                        }
                    }
                }
                LocalMcpTransportConfig::StreamableHttp2026 { endpoint, .. } => {
                    let direct = local_direct_server(server, endpoint)?;
                    let arguments_json = arguments.to_string();
                    let call = self.http.call_tool_round(
                        identity.tenant_id,
                        &direct,
                        qualified_name,
                        &arguments_json,
                        frozen_catalog_digest,
                        None,
                    );
                    let outcome = tokio::select! {
                        biased;
                        () = context.cancellation.cancelled() => {
                            return Err(McpGatewayClientError::Cancelled);
                        }
                        outcome = call => outcome.map_err(local_mcp_error)?,
                    };
                    match outcome {
                        DirectMcpToolCallOutcome::Complete(result) => result,
                        DirectMcpToolCallOutcome::InputRequired(_) => {
                            return Err(McpGatewayClientError::InvalidResponse(
                                "MCP Tool requires the continuation-aware execution path".into(),
                            ));
                        }
                    }
                }
                LocalMcpTransportConfig::Stdio { .. }
                | LocalMcpTransportConfig::StdioV20250326 { .. } => self
                    .stdio
                    .call_tool_with_lifecycle(
                        server.server_id,
                        &server.name,
                        qualified_name,
                        arguments,
                        frozen_catalog_digest,
                        context,
                    )
                    .await
                    .map_err(local_mcp_error)?,
                LocalMcpTransportConfig::Stdio2026 { .. } => {
                    return Err(McpGatewayClientError::InvalidResponse(
                        "MCP 2026 Tool requires the continuation-aware execution path".into(),
                    ));
                }
            };
            let content = serde_json::from_str(&result.content_json)
                .map_err(|error| McpGatewayClientError::InvalidResponse(error.to_string()))?;
            Ok((content, result.is_error))
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the federation trust boundary keeps identity, authority and lifecycle inputs explicit"
    )]
    fn call_tool_round<'a>(
        &'a self,
        identity: &'a FederationIdentity,
        server: &'a agent_protocol::McpServerSnapshot,
        qualified_name: &'a str,
        arguments: &'a serde_json::Value,
        frozen_catalog_digest: &'a str,
        workload_token: &'a str,
        context: &'a McpCallContext,
        continuation: Option<&'a McpInputContinuation>,
    ) -> Pin<Box<dyn Future<Output = Result<McpToolRoundOutcome, McpGatewayClientError>> + Send + 'a>>
    {
        Box::pin(async move {
            let transport = self.transport(server)?;
            if !matches!(
                transport,
                LocalMcpTransportConfig::StreamableHttp2026 { .. }
                    | LocalMcpTransportConfig::Stdio2026 { .. }
            ) {
                if continuation.is_some() {
                    return Err(McpGatewayClientError::InvalidResponse(
                        "legacy MCP transport cannot resume a stateless continuation".into(),
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
                return Ok(McpToolRoundOutcome::Complete { content, is_error });
            }
            if matches!(transport, LocalMcpTransportConfig::Stdio2026 { .. }) {
                let outcome = self
                    .stdio
                    .call_tool_round_with_lifecycle(
                        server.server_id,
                        &server.name,
                        qualified_name,
                        arguments,
                        frozen_catalog_digest,
                        context,
                        continuation,
                    )
                    .await
                    .map_err(local_mcp_error)?;
                return match outcome {
                    DirectMcpToolCallOutcome::Complete(result) => {
                        let content =
                            serde_json::from_str(&result.content_json).map_err(|error| {
                                McpGatewayClientError::InvalidResponse(error.to_string())
                            })?;
                        Ok(McpToolRoundOutcome::Complete {
                            content,
                            is_error: result.is_error,
                        })
                    }
                    DirectMcpToolCallOutcome::InputRequired(required) => {
                        Ok(McpToolRoundOutcome::InputRequired {
                            round: required.round,
                            request_state: required.request_state,
                            requests: required.requests,
                        })
                    }
                };
            }
            let LocalMcpTransportConfig::StreamableHttp2026 { endpoint, .. } = transport else {
                unreachable!("modern transports were handled above")
            };
            let direct = local_direct_server(server, endpoint)?;
            let direct_continuation =
                continuation.map(|continuation| DirectMcpRoundTripContinuation {
                    round: continuation.round,
                    request_state: continuation.request_state.clone(),
                    responses: continuation.responses.clone(),
                });
            let arguments_json = arguments.to_string();
            let call = self.http.call_tool_round(
                identity.tenant_id,
                &direct,
                qualified_name,
                &arguments_json,
                frozen_catalog_digest,
                direct_continuation.as_ref(),
            );
            let outcome = tokio::select! {
                biased;
                () = context.cancellation.cancelled() => {
                    return Err(McpGatewayClientError::Cancelled);
                }
                outcome = call => outcome.map_err(local_mcp_error)?,
            };
            match outcome {
                DirectMcpToolCallOutcome::Complete(result) => {
                    let content = serde_json::from_str(&result.content_json).map_err(|error| {
                        McpGatewayClientError::InvalidResponse(error.to_string())
                    })?;
                    Ok(McpToolRoundOutcome::Complete {
                        content,
                        is_error: result.is_error,
                    })
                }
                DirectMcpToolCallOutcome::InputRequired(required) => {
                    Ok(McpToolRoundOutcome::InputRequired {
                        round: required.round,
                        request_state: required.request_state,
                        requests: required.requests,
                    })
                }
            }
        })
    }
}

fn local_direct_server(
    server: &agent_protocol::McpServerSnapshot,
    endpoint: &str,
) -> Result<DirectMcpServerRef, McpGatewayClientError> {
    if !server.credential_envelope_base64.is_empty() || server.oauth_credential_id.is_some() {
        return Err(McpGatewayClientError::InvalidResponse(
            "standalone MCP cannot resolve a credential-domain credential".into(),
        ));
    }
    Ok(DirectMcpServerRef {
        server_id: server.server_id,
        name: server.name.clone(),
        endpoint: endpoint.to_owned(),
        credential_envelope_json: String::new(),
        oauth_credential_id: None,
        protocol_revision: server.protocol_revision,
        client_capabilities: server.client_capabilities.clone(),
    })
}

fn local_mcp_error(error: McpFederationError) -> McpGatewayClientError {
    match error {
        McpFederationError::Cancelled => McpGatewayClientError::Cancelled,
        McpFederationError::Unreachable(message) => McpGatewayClientError::Transport(message),
        other => McpGatewayClientError::InvalidResponse(other.to_string()),
    }
}

#[derive(Clone, Debug)]
pub struct LocalProcessSessionConfig {
    /// Explicit trusted executable to keep alive across Tool calls. The host
    /// never installs an implicit shell.
    pub executable: PathBuf,
    pub fixed_args: Vec<String>,
    pub max_output_chunk_bytes: usize,
    pub governance: ProcessSessionGovernance,
    pub pty_supervisor: Option<ProcessSessionPtySupervisorConfig>,
}

#[derive(Clone, Debug)]
pub struct LocalRuntimeConfig {
    pub state_root: PathBuf,
    pub workspace_root: PathBuf,
    pub agent_instructions: String,
    pub delegated_scopes: BTreeSet<String>,
    /// Roles the primary Agent may delegate to. A child receives the selected
    /// role's instructions and only that role's delegated scope subset.
    pub subagent_roles: Vec<SubagentRole>,
    pub model_routing: LocalModelRoutingConfig,
    pub mcp_servers: Vec<LocalMcpServerConfig>,
    pub mcp_lifecycle: LocalMcpLifecycleConfig,
    /// Absolute path to the trusted workspace Tool binary. Without it the host
    /// installs no Tools at all rather than falling back to anything untrusted.
    pub trusted_workspace_tool: Option<PathBuf>,
    /// Optional persistent process Tool family. `None` means the model cannot
    /// start or interact with a long-lived local process.
    pub process_session: Option<LocalProcessSessionConfig>,
    pub consent: LocalToolConsent,
    pub budget: RunBudget,
    /// Host-level execution semantics frozen into every local Run before it is
    /// accepted. Recovery compares this exact snapshot with the Checkpoint.
    pub runtime_policy: RuntimeExecutionPolicySnapshot,
}

/// Durable lifecycle of a local Run. Without it a restarted daemon cannot tell
/// a Run that is still owed work from one that already finished, and would
/// either re-execute completed Runs or abandon live ones.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LocalRunState {
    Running,
    /// The operator's cancellation was durably acknowledged, but the owning
    /// execution has not yet emitted its terminal Kernel event. A replacement
    /// daemon must finish this intent and must never resume ordinary work.
    Cancelling {
        reason: String,
    },
    /// Parked on the approval gate. Deliberately not `Finished`: a Run waiting
    /// for a human is still owed work, and recording it as finished would make
    /// recovery skip it forever and leave it permanently unapprovable.
    AwaitingApproval {
        approval_id: Uuid,
        binding_digest: String,
        /// Nil/absent in legacy local records means the root Run itself. New
        /// records always persist the exact Run whose Tool is blocked.
        #[serde(default)]
        target_run_id: Option<Uuid>,
    },
    /// A reviewer decision was durably acknowledged but has not yet been
    /// consumed by the exact Checkpoint-bound Tool call. Recovery must replay
    /// this decision, never invoke the model without it.
    ApprovalDecided {
        target_run_id: Uuid,
        approval_id: Uuid,
        binding_digest: String,
        decision: LocalApprovalDecision,
    },
    AwaitingMcpInput {
        input: McpInputRequired,
    },
    McpInputDecided {
        resolution: LocalMcpInputResolution,
    },
    Finished {
        status: String,
    },
    Cancelled {
        reason: String,
    },
    /// The daemon died before the Run produced a Checkpoint, so there is
    /// nothing to resume from and re-running is not automatically safe.
    Interrupted {
        reason: String,
    },
}

/// The durable record a restarted daemon reads to decide what to do.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LocalRunRecord {
    pub store_version: u32,
    #[serde(default)]
    pub tenant_id: Uuid,
    #[serde(default)]
    pub application_id: Uuid,
    #[serde(default)]
    pub workload_identity_id: Uuid,
    #[serde(default)]
    pub workspace_id: Uuid,
    #[serde(default)]
    pub agent_version_id: Uuid,
    #[serde(default)]
    pub model_policy_id: Uuid,
    pub run_id: Uuid,
    pub input: String,
    pub state: LocalRunState,
    /// Highest owner epoch used so far. Recovery must exceed it, otherwise the
    /// Checkpoint is refused as a stale lease.
    pub owner_epoch: u64,
}

/// The approval a parked Run is waiting on. Both fields are needed to answer
/// it: the id names the decision, the binding digest proves the decision
/// applies to the exact Tool call that was planned.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalPendingApproval {
    pub target_run_id: Uuid,
    pub approval_id: Uuid,
    pub binding_digest: String,
}

#[derive(Clone, Debug)]
pub(crate) struct LocalApprovalResolution {
    target_run_id: Uuid,
    approval_id: Option<Uuid>,
    binding_digest: Option<String>,
    decision: LocalApprovalDecision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalMcpInputResolution {
    pub input_id: Uuid,
    pub input_version: u32,
    pub binding_digest: String,
    pub responses: BTreeMap<String, McpInputResponse>,
}

#[derive(Deserialize)]
struct CheckpointResolvedMcpInput {
    pending: McpInputRequired,
    continuation: McpInputContinuation,
}

enum LocalResumeResolution {
    Approval(LocalApprovalResolution),
    McpInput(LocalMcpInputResolution),
}

enum LocalSubagentProgress {
    Completed(SubagentResultDelivery),
    AwaitingApproval(LocalPendingApproval),
}

struct LocalSubagentBatchProgress {
    results: Vec<SubagentResultDelivery>,
    pending_approval: Option<LocalPendingApproval>,
}

struct LocalSubagentTask {
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<Result<LocalSubagentProgress, LocalRuntimeError>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalSubagentWaitArguments {
    agent_id: Uuid,
    timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalSubagentCloseArguments {
    agent_id: Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalSubagentSendArguments {
    agent_id: Uuid,
    #[serde(default)]
    generation: Option<u64>,
    message: String,
    idempotency_key: String,
    #[serde(default)]
    interrupt: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalSubagentHistoryArguments {
    agent_id: Uuid,
    #[serde(default)]
    generation: Option<u64>,
    #[serde(default)]
    after_activation_ordinal: Option<u64>,
    limit: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalSubagentRollbackArguments {
    agent_id: Uuid,
    generation: u64,
    through_activation_ordinal: u64,
}

/// One durable Run event as local clients see it. Persisted to the Run's event
/// log before it is broadcast, so a client that reconnects can replay exactly
/// what a client that stayed connected already received.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LocalEvent {
    /// Stable event identity from the Kernel envelope. Older local logs did not
    /// persist it, so recovery accepts a nil default for those historical rows.
    #[serde(default)]
    pub event_id: Uuid,
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub tenant_id: Uuid,
    #[serde(default)]
    pub application_id: Uuid,
    #[serde(default)]
    pub workload_identity_id: Uuid,
    #[serde(default)]
    pub workspace_id: Uuid,
    #[serde(default)]
    pub agent_version_id: Uuid,
    #[serde(default)]
    pub model_policy_id: Uuid,
    #[serde(default)]
    pub session_id: Uuid,
    pub sequence: u64,
    pub run_id: Uuid,
    #[serde(default)]
    pub attempt_id: Uuid,
    #[serde(default)]
    pub timestamp: chrono::DateTime<Utc>,
    #[serde(default)]
    pub trace_id: Uuid,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: serde_json::Value,
    #[serde(default)]
    pub digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalRunOutcome {
    pub run_id: Uuid,
    pub attempt_id: Uuid,
    pub status: RunStatus,
    pub event_types: Vec<String>,
    pub output: String,
    pub checkpoint_path: PathBuf,
    /// Set when execution stopped on an approval the operator has not answered.
    pub pending_approval: Option<LocalPendingApproval>,
    /// Set when an MCP 2026 Tool returned a bounded stateless input request.
    pub pending_mcp_input: Option<McpInputRequired>,
    /// Every configured MCP server in command order. Optional failures remain
    /// visible here even though they do not reject the Run.
    pub mcp_servers: Vec<McpServerDiscoveryStatus>,
    /// Present only when the caller used the explicit lower-authority history
    /// import API. Digests and counts are durable in the Worker Checkpoint.
    pub history_repair: Option<HistoryRepairReport>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalToolReconciliationOutcome {
    pub source_run_id: Uuid,
    pub reconciliation_id: Uuid,
    pub version: u32,
    pub decision: ToolReconciliationDecision,
    pub continuation: Option<LocalRunOutcome>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LocalToolReconciliationRecord {
    schema_version: u32,
    reconciliation_id: Uuid,
    versions: BTreeMap<u32, ToolReconciliationCommand>,
    continuation_outcome: Option<LocalRunOutcome>,
}

/// Durable caller-visible head of one root Session branch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalSessionHead {
    pub session_id: Uuid,
    pub branch_id: Uuid,
    pub generation: u64,
    pub turn_count: u64,
    pub history_digest: String,
    pub active_run_id: Option<Uuid>,
}

#[derive(Clone, Debug)]
pub struct LocalSessionRunOutcome {
    pub run: LocalRunOutcome,
    pub head: LocalSessionHead,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LocalSessionTurnBinding {
    pub invocation: RuntimeInvocationContext,
    pub session_id: Uuid,
    pub branch_id: Uuid,
    pub generation: u64,
    pub run_id: Uuid,
    pub input: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LocalSessionTurnPreparation {
    Execute(LocalSessionHead),
    Existing(LocalSessionHead),
}

/// The read-only half of accepting a Turn.
///
/// Separated from the write so a retry can be answered before any quota is
/// taken, and so ownership and admission are settled before anything durable
/// records that a Turn is in flight. Deciding and writing in one step is what
/// left a refused Turn holding a branch open against a Run that was never
/// created.
#[derive(Clone, Debug)]
pub(crate) enum LocalSessionTurnDecision {
    /// Already accepted, active or completed. The caller answers from the
    /// current receipt rather than from anything carried here, and takes no run
    /// capacity to do it.
    Existing,
    /// Not seen before, and admissible as far as the Session store is
    /// concerned. The caller must take ownership and admission before writing.
    New,
}

const LOCAL_SESSION_STORE_VERSION: u32 = 2;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LocalSessionActiveTurn {
    run_id: Uuid,
    generation: u64,
    history_digest: String,
    input: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LocalSessionBranchRecord {
    branch_id: Uuid,
    generation: u64,
    history: Vec<SessionConversationTurn>,
    archived_generations: BTreeMap<u64, Vec<SessionConversationTurn>>,
    active_turn: Option<LocalSessionActiveTurn>,
}

impl LocalSessionBranchRecord {
    fn snapshot(&self) -> SessionBranchSnapshot {
        SessionBranchSnapshot::new(self.branch_id, self.generation, self.history.clone())
    }

    fn head(&self, session_id: Uuid) -> LocalSessionHead {
        let snapshot = self.snapshot();
        LocalSessionHead {
            session_id,
            branch_id: self.branch_id,
            generation: self.generation,
            turn_count: u64::try_from(self.history.len()).unwrap_or(u64::MAX),
            history_digest: snapshot.history_digest,
            active_run_id: self.active_turn.as_ref().map(|active| active.run_id),
        }
    }

    fn is_well_formed(&self) -> bool {
        let current = self.snapshot();
        current.is_well_formed()
            && u64::try_from(self.archived_generations.len()).unwrap_or(u64::MAX)
                == self.generation.saturating_sub(1)
            && self
                .archived_generations
                .iter()
                .enumerate()
                .all(|(index, (generation, history))| {
                    *generation == u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1)
                        && SessionBranchSnapshot::new(self.branch_id, *generation, history.clone())
                            .is_well_formed()
                })
            && self.active_turn.as_ref().is_none_or(|active| {
                !active.run_id.is_nil()
                    && active.generation == self.generation
                    && active.history_digest == current.history_digest
                    && !active.input.trim().is_empty()
                    && active.input.len() <= 32_000
            })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LocalSessionRecord {
    store_version: u32,
    #[serde(default = "crate::local_invocation_context")]
    invocation: RuntimeInvocationContext,
    session_id: Uuid,
    branches: BTreeMap<Uuid, LocalSessionBranchRecord>,
}

impl LocalSessionRecord {
    fn is_well_formed(&self) -> bool {
        (self.store_version == LOCAL_SESSION_STORE_VERSION
            || (self.store_version == 1 && self.invocation == crate::local_invocation_context()))
            && self.invocation.validate().is_ok()
            && !self.session_id.is_nil()
            && !self.branches.is_empty()
            && self.branches.iter().all(|(branch_id, branch)| {
                *branch_id == branch.branch_id && branch.is_well_formed()
            })
    }
}

pub struct LocalRuntimeHost {
    config: LocalRuntimeConfig,
    invocation: RuntimeInvocationContext,
    processor: WorkerProcessor,
    model_routes: Vec<LocalModelRoute>,
    routing_constraints: RoutingConstraints,
    model_route_binding_digest: String,
    provider_health: Arc<StdMutex<LocalProviderHealthStore>>,
    mcp_client: Option<McpFederationClient>,
    stdio_mcp: Option<StdioMcpClient>,
    executors: std::collections::HashMap<String, Arc<dyn ToolExecutor>>,
    process_session_manager: Option<Arc<PersistentProcessSessionManager>>,
    worker_id: Uuid,
    /// Root of this Run's cancellation tree. A child receives a child token,
    /// so cancellation propagates downward without letting it cancel a parent.
    cancellation: CancellationToken,
    /// Set only by this Host's duration watchdog. Ancestor/user cancellation
    /// therefore remains distinguishable from a local duration terminal.
    duration_expired: Arc<AtomicBool>,
    /// Live tasks are a cache of durable handle state. After a daemon restart,
    /// `agent.wait` recreates a missing task from the parent and child
    /// Checkpoints rather than treating process memory as authority.
    subagent_tasks: HashMap<Uuid, LocalSubagentTask>,
    pending_mcp_input: Option<McpInputRequired>,
}

impl Drop for LocalRuntimeHost {
    fn drop(&mut self) {
        // A JoinHandle detaches its task when dropped. Cancel the Host-owned
        // subtree and abort any still-registered child tasks so an unwound or
        // aborted owner cannot leave Provider connections running unowned.
        self.cancellation.cancel();
        for task in self.subagent_tasks.values() {
            task.cancellation.cancel();
            task.task.abort();
        }
    }
}

#[derive(Clone)]
struct LocalModelRoute {
    candidate: ModelCandidate,
    protocol: ProviderProtocol,
    endpoint: String,
    model: String,
    adapter: ProviderAdapter,
    credential: ProviderCredential,
}

const LOCAL_PROVIDER_HEALTH_STORE_VERSION: u32 = 1;
const LOCAL_PROVIDER_HEALTH_STORE_MAX_ENTRIES: usize = 256;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LocalProviderHealthEntry {
    route_binding_digest: String,
    provider_id: String,
    consecutive_failures: u8,
    cooldown_until_unix_ms: Option<i64>,
    probe_owner_invocation_digest: Option<String>,
    probe_lease_until_unix_ms: Option<i64>,
    last_failure_kind: Option<ModelErrorKind>,
    last_failure_status: Option<u16>,
    updated_at_unix_ms: i64,
}

impl LocalProviderHealthEntry {
    fn is_well_formed(&self) -> bool {
        self.route_binding_digest.len() == 64
            && !self.provider_id.trim().is_empty()
            && self.provider_id.len() <= 128
            && self.cooldown_until_unix_ms.is_none_or(|value| value >= 0)
            && self
                .probe_owner_invocation_digest
                .as_ref()
                .is_none_or(|digest| digest.len() == 64)
            && self
                .probe_lease_until_unix_ms
                .is_none_or(|value| value >= 0)
            && (self.probe_owner_invocation_digest.is_some()
                == self.probe_lease_until_unix_ms.is_some())
            && self.updated_at_unix_ms >= 0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct LocalProviderHealthStore {
    store_version: u32,
    entries: BTreeMap<String, LocalProviderHealthEntry>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocalProviderAdmission {
    Closed,
    HalfOpenProbe,
    Skip,
}

impl LocalProviderHealthStore {
    fn key(route_binding_digest: &str, provider_id: &str) -> String {
        format!("{route_binding_digest}:{provider_id}")
    }

    fn load(state_root: &Path) -> Result<Self, LocalRuntimeError> {
        let path = state_root.join("model-provider-health.json");
        if !path.exists() {
            return Ok(Self {
                store_version: LOCAL_PROVIDER_HEALTH_STORE_VERSION,
                entries: BTreeMap::new(),
                path,
            });
        }
        let body = std::fs::read(&path)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        let mut store: Self = serde_json::from_slice(&body)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        store.path = path;
        if !store.is_well_formed() {
            return Err(LocalRuntimeError::Checkpoint(
                "local Provider health store is malformed".into(),
            ));
        }
        Ok(store)
    }

    fn is_well_formed(&self) -> bool {
        self.store_version == LOCAL_PROVIDER_HEALTH_STORE_VERSION
            && self.entries.len() <= LOCAL_PROVIDER_HEALTH_STORE_MAX_ENTRIES
            && self.entries.iter().all(|(key, entry)| {
                *key == Self::key(&entry.route_binding_digest, &entry.provider_id)
                    && entry.is_well_formed()
            })
    }

    fn persist(&self) -> Result<(), LocalRuntimeError> {
        if !self.is_well_formed() {
            return Err(LocalRuntimeError::Checkpoint(
                "refusing to persist malformed local Provider health state".into(),
            ));
        }
        let staging = self.path.with_extension("json.partial");
        std::fs::write(
            &staging,
            serde_json::to_vec_pretty(self)
                .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?,
        )
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        std::fs::rename(staging, &self.path)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))
    }

    fn prune_before_insert(&mut self) {
        if self.entries.len() < LOCAL_PROVIDER_HEALTH_STORE_MAX_ENTRIES {
            return;
        }
        if let Some(oldest) = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.updated_at_unix_ms)
            .map(|(key, _)| key.clone())
        {
            self.entries.remove(&oldest);
        }
    }

    fn admission(
        &mut self,
        route_binding_digest: &str,
        provider_id: &str,
        invocation_digest: &str,
        policy: &LocalProviderHealthPolicy,
    ) -> Result<LocalProviderAdmission, LocalRuntimeError> {
        let key = Self::key(route_binding_digest, provider_id);
        let Some(entry) = self.entries.get_mut(&key) else {
            return Ok(LocalProviderAdmission::Closed);
        };
        let now = Utc::now().timestamp_millis();
        if entry
            .cooldown_until_unix_ms
            .is_some_and(|deadline| deadline > now)
        {
            return Ok(LocalProviderAdmission::Skip);
        }
        if entry.consecutive_failures < policy.consecutive_failure_threshold {
            return Ok(LocalProviderAdmission::Closed);
        }
        if entry.probe_owner_invocation_digest.as_deref() == Some(invocation_digest)
            && entry
                .probe_lease_until_unix_ms
                .is_some_and(|deadline| deadline > now)
        {
            return Ok(LocalProviderAdmission::HalfOpenProbe);
        }
        if entry
            .probe_lease_until_unix_ms
            .is_some_and(|deadline| deadline > now)
        {
            return Ok(LocalProviderAdmission::Skip);
        }
        entry.probe_owner_invocation_digest = Some(invocation_digest.to_owned());
        entry.probe_lease_until_unix_ms = Some(
            now.saturating_add(i64::try_from(policy.half_open_probe_lease_ms).unwrap_or(i64::MAX)),
        );
        entry.updated_at_unix_ms = now;
        self.persist()?;
        Ok(LocalProviderAdmission::HalfOpenProbe)
    }

    fn is_eligible_for_new_route(
        &self,
        route_binding_digest: &str,
        provider_id: &str,
        invocation_digest: &str,
        policy: &LocalProviderHealthPolicy,
    ) -> bool {
        let key = Self::key(route_binding_digest, provider_id);
        let Some(entry) = self.entries.get(&key) else {
            return true;
        };
        let now = Utc::now().timestamp_millis();
        if entry
            .cooldown_until_unix_ms
            .is_some_and(|deadline| deadline > now)
        {
            return false;
        }
        if entry.consecutive_failures < policy.consecutive_failure_threshold {
            return true;
        }
        entry.probe_owner_invocation_digest.as_deref() == Some(invocation_digest)
            || entry
                .probe_lease_until_unix_ms
                .is_none_or(|deadline| deadline <= now)
    }

    fn observe_success(
        &mut self,
        route_binding_digest: &str,
        provider_id: &str,
    ) -> Result<(), LocalRuntimeError> {
        let key = Self::key(route_binding_digest, provider_id);
        if self.entries.remove(&key).is_some() {
            self.persist()?;
        }
        Ok(())
    }

    fn observe_failure(
        &mut self,
        route_binding_digest: &str,
        provider_id: &str,
        invocation_digest: &str,
        failure: &LocalModelRouteFailure,
        policy: &LocalProviderHealthPolicy,
    ) -> Result<bool, LocalRuntimeError> {
        let transient = failure.retryable
            && matches!(
                failure.kind,
                ModelErrorKind::RateLimited | ModelErrorKind::Timeout | ModelErrorKind::Unavailable
            );
        let key = Self::key(route_binding_digest, provider_id);
        if !transient {
            if let Some(entry) = self.entries.get_mut(&key)
                && entry.probe_owner_invocation_digest.as_deref() == Some(invocation_digest)
            {
                entry.probe_owner_invocation_digest = None;
                entry.probe_lease_until_unix_ms = None;
                entry.updated_at_unix_ms = Utc::now().timestamp_millis();
                self.persist()?;
            }
            return Ok(false);
        }
        if !self.entries.contains_key(&key) {
            self.prune_before_insert();
            self.entries.insert(
                key.clone(),
                LocalProviderHealthEntry {
                    route_binding_digest: route_binding_digest.to_owned(),
                    provider_id: provider_id.to_owned(),
                    consecutive_failures: 0,
                    cooldown_until_unix_ms: None,
                    probe_owner_invocation_digest: None,
                    probe_lease_until_unix_ms: None,
                    last_failure_kind: None,
                    last_failure_status: None,
                    updated_at_unix_ms: Utc::now().timestamp_millis(),
                },
            );
        }
        let entry = self.entries.get_mut(&key).expect("entry was inserted");
        let now = Utc::now().timestamp_millis();
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.last_failure_kind = Some(failure.kind);
        entry.last_failure_status = failure.status;
        entry.probe_owner_invocation_digest = None;
        entry.probe_lease_until_unix_ms = None;
        let retry_after = failure
            .retry_after_ms
            .map(|delay| delay.min(policy.max_retry_after_ms));
        let opened = entry.consecutive_failures >= policy.consecutive_failure_threshold
            || retry_after.is_some();
        if opened {
            let delay = retry_after.unwrap_or(policy.cooldown_ms);
            entry.cooldown_until_unix_ms =
                Some(now.saturating_add(i64::try_from(delay).unwrap_or(i64::MAX)));
        }
        entry.updated_at_unix_ms = now;
        self.persist()?;
        Ok(opened)
    }
}

type SharedLocalProviderHealth = Arc<StdMutex<LocalProviderHealthStore>>;

static LOCAL_PROVIDER_HEALTH_REGISTRY: OnceLock<
    StdMutex<HashMap<PathBuf, Weak<StdMutex<LocalProviderHealthStore>>>>,
> = OnceLock::new();

fn shared_local_provider_health(
    state_root: &Path,
) -> Result<SharedLocalProviderHealth, LocalRuntimeError> {
    let root = std::fs::canonicalize(state_root)
        .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
    let registry = LOCAL_PROVIDER_HEALTH_REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .map_err(|_| LocalRuntimeError::StateRoot("Provider health registry is poisoned".into()))?;
    if let Some(shared) = registry.get(&root).and_then(Weak::upgrade) {
        return Ok(shared);
    }
    let shared = Arc::new(StdMutex::new(LocalProviderHealthStore::load(&root)?));
    registry.insert(root, Arc::downgrade(&shared));
    Ok(shared)
}

const LOCAL_MODEL_ROUTE_STORE_VERSION: u32 = 3;
const LOCAL_MODEL_ROUTE_WAL_RECORD_VERSION: u32 = 1;
const LOCAL_MODEL_ROUTE_WAL_MAX_RECORDS: usize = 32;
const LOCAL_MODEL_ROUTE_WAL_MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
const LOCAL_MODEL_ROUTE_WAL_MAX_FILE_BYTES: u64 =
    ((LOCAL_MODEL_ROUTE_WAL_MAX_RECORDS + 1) * LOCAL_MODEL_ROUTE_WAL_MAX_LINE_BYTES) as u64;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct LocalModelRouteFailure {
    provider_id: String,
    kind: ModelErrorKind,
    retryable: bool,
    status: Option<u16>,
    #[serde(default)]
    retry_after_ms: Option<u64>,
    message_digest: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct LocalModelRouteRetry {
    provider_id: String,
    provider_attempt: u8,
    failure: LocalModelRouteFailure,
    delay_ms: u64,
    retry_not_before_unix_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct LocalModelRouteJournal {
    store_version: u32,
    run_id: Uuid,
    attempt_id: Uuid,
    invocation_digest: String,
    model_route_binding_digest: String,
    candidate_ids: Vec<String>,
    next_candidate_index: usize,
    failed_attempts: Vec<LocalModelRouteFailure>,
    reported_failure_count: usize,
    #[serde(default)]
    retry_attempts: Vec<LocalModelRouteRetry>,
    #[serde(default)]
    reported_retry_count: usize,
    #[serde(default)]
    same_provider_attempts: u8,
    #[serde(default)]
    inflight_provider_id: Option<String>,
    #[serde(default)]
    retry_not_before_unix_ms: Option<i64>,
    selected_provider_id: Option<String>,
    selection_reported: bool,
    terminal_failure: Option<LocalModelRouteFailure>,
    terminal_failure_reported: bool,
    staged_events: Vec<ModelStreamEvent>,
    completed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LocalModelRouteWalRecord {
    record_version: u32,
    revision: u64,
    journal: LocalModelRouteJournal,
}

impl LocalModelRouteWalRecord {
    fn is_well_formed(&self) -> bool {
        self.record_version == LOCAL_MODEL_ROUTE_WAL_RECORD_VERSION
            && self.revision > 0
            && self.journal.store_version == LOCAL_MODEL_ROUTE_STORE_VERSION
            && self.journal.is_well_formed()
    }

    fn follows(&self, previous: &Self) -> bool {
        self.journal.run_id == previous.journal.run_id
            && self.journal.invocation_digest == previous.journal.invocation_digest
            && self.journal.model_route_binding_digest
                == previous.journal.model_route_binding_digest
            && self.journal.candidate_ids == previous.journal.candidate_ids
            && self.journal.next_candidate_index >= previous.journal.next_candidate_index
            && self
                .journal
                .failed_attempts
                .starts_with(&previous.journal.failed_attempts)
            && self
                .journal
                .retry_attempts
                .starts_with(&previous.journal.retry_attempts)
            && self.journal.reported_failure_count >= previous.journal.reported_failure_count
            && self.journal.reported_retry_count >= previous.journal.reported_retry_count
            && (self.journal.next_candidate_index > previous.journal.next_candidate_index
                || self.journal.same_provider_attempts >= previous.journal.same_provider_attempts)
            && (previous.journal.inflight_provider_id.is_none()
                || self.journal.inflight_provider_id.is_none())
            && previous
                .journal
                .selected_provider_id
                .as_ref()
                .is_none_or(|selected| self.journal.selected_provider_id.as_ref() == Some(selected))
            && (!previous.journal.selection_reported || self.journal.selection_reported)
            && (!previous.journal.completed || self.journal.completed)
            && (previous.journal.staged_events.is_empty()
                || self.journal.staged_events == previous.journal.staged_events
                || (self.journal.completed && self.journal.staged_events.is_empty()))
            && match (
                previous.journal.terminal_failure.as_ref(),
                self.journal.terminal_failure.as_ref(),
            ) {
                (Some(previous_failure), Some(current_failure)) => {
                    previous_failure == current_failure
                        && (!previous.journal.terminal_failure_reported
                            || self.journal.terminal_failure_reported)
                }
                (Some(_), None) => {
                    previous.journal.terminal_failure_reported
                        && !self.journal.terminal_failure_reported
                        && self.journal.attempt_id != previous.journal.attempt_id
                }
                (None, _) => true,
            }
    }
}

impl LocalModelRouteJournal {
    fn failures_follow_candidate_order(&self) -> bool {
        let mut previous_index = None;
        self.failed_attempts.iter().all(|failure| {
            let Some(index) = self
                .candidate_ids
                .iter()
                .position(|candidate_id| candidate_id == &failure.provider_id)
            else {
                return false;
            };
            let ordered = previous_index.is_none_or(|previous| index > previous);
            previous_index = Some(index);
            ordered && index < self.next_candidate_index && failure.message_digest.len() == 64
        })
    }

    fn is_well_formed(&self) -> bool {
        matches!(self.store_version, 1..=LOCAL_MODEL_ROUTE_STORE_VERSION)
            && !self.run_id.is_nil()
            && !self.attempt_id.is_nil()
            && self.invocation_digest.len() == 64
            && self.model_route_binding_digest.len() == 64
            && !self.candidate_ids.is_empty()
            && self.candidate_ids.len() <= 8
            && self
                .candidate_ids
                .iter()
                .all(|id| !id.trim().is_empty() && id.len() <= 128)
            && self.candidate_ids.iter().collect::<BTreeSet<_>>().len() == self.candidate_ids.len()
            && self.next_candidate_index < self.candidate_ids.len()
            && self.failed_attempts.len() <= self.next_candidate_index
            && self.reported_failure_count <= self.failed_attempts.len()
            && self.retry_attempts.len() <= 32
            && self.reported_retry_count <= self.retry_attempts.len()
            && self.same_provider_attempts <= 4
            && self
                .inflight_provider_id
                .as_ref()
                .is_none_or(|provider_id| {
                    !self.completed
                        && self.staged_events.is_empty()
                        && self.candidate_ids.get(self.next_candidate_index) == Some(provider_id)
                        && self.same_provider_attempts > 0
                })
            && self.retry_not_before_unix_ms.is_none_or(|not_before| {
                !self.completed && not_before >= 0 && self.same_provider_attempts > 0
            })
            && self.retry_attempts.iter().all(|retry| {
                self.candidate_ids.contains(&retry.provider_id)
                    && retry.provider_attempt > 0
                    && retry.provider_attempt <= 4
                    && retry.failure.provider_id == retry.provider_id
                    && retry.failure.message_digest.len() == 64
                    && retry.delay_ms <= 3_600_000
                    && retry.retry_not_before_unix_ms >= 0
            })
            && self.failures_follow_candidate_order()
            && self.selected_provider_id.as_ref().is_none_or(|selected| {
                self.candidate_ids.get(self.next_candidate_index) == Some(selected)
            })
            && (!self.selection_reported || self.selected_provider_id.is_some())
            && self.terminal_failure.as_ref().is_none_or(|failure| {
                self.candidate_ids.get(self.next_candidate_index) == Some(&failure.provider_id)
                    && failure.message_digest.len() == 64
            })
            && (!self.terminal_failure_reported || self.terminal_failure.is_some())
            && !(self.selected_provider_id.is_some() && self.terminal_failure.is_some())
            && (!self.completed
                || (self.staged_events.is_empty()
                    && ((self.selected_provider_id.is_some() && self.selection_reported)
                        || (self.terminal_failure.is_some() && self.terminal_failure_reported))))
    }
}

#[cfg(test)]
mod model_route_wal_tests {
    use super::*;

    fn journal() -> LocalModelRouteJournal {
        LocalModelRouteJournal {
            store_version: LOCAL_MODEL_ROUTE_STORE_VERSION,
            run_id: Uuid::now_v7(),
            attempt_id: Uuid::now_v7(),
            invocation_digest: "11".repeat(32),
            model_route_binding_digest: "22".repeat(32),
            candidate_ids: vec!["primary".into()],
            next_candidate_index: 0,
            failed_attempts: Vec::new(),
            reported_failure_count: 0,
            retry_attempts: Vec::new(),
            reported_retry_count: 0,
            same_provider_attempts: 0,
            inflight_provider_id: None,
            retry_not_before_unix_ms: None,
            selected_provider_id: None,
            selection_reported: false,
            terminal_failure: None,
            terminal_failure_reported: false,
            staged_events: Vec::new(),
            completed: false,
        }
    }

    #[test]
    fn model_route_wal_repairs_only_an_uncommitted_tail_before_the_next_append() {
        let root = tempfile::tempdir().expect("state");
        let path = root.path().join("route.json");
        let first = journal();
        LocalRuntimeHost::persist_model_route_journal(&path, &first).expect("first commit");
        let mut second = first.clone();
        second.same_provider_attempts = 1;
        second.inflight_provider_id = Some("primary".into());
        LocalRuntimeHost::persist_model_route_journal(&path, &second).expect("second commit");
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("WAL")
            .write_all(b"{\"record_version\":1")
            .expect("torn tail");

        assert_eq!(
            LocalRuntimeHost::read_model_route_journal(&path).expect("committed prefix"),
            second
        );
        LocalRuntimeHost::persist_model_route_journal(&path, &second).expect("repair then append");
        let body = std::fs::read(&path).expect("repaired WAL");
        assert!(body.ends_with(b"\n"));
        assert_eq!(
            LocalRuntimeHost::read_model_route_wal_records(&path)
                .expect("records")
                .len(),
            2,
            "an idempotent retry repairs the tail without adding a duplicate record"
        );
    }

    #[test]
    fn committed_model_route_wal_corruption_fails_closed() {
        let root = tempfile::tempdir().expect("state");
        let path = root.path().join("route.json");
        LocalRuntimeHost::persist_model_route_journal(&path, &journal()).expect("first commit");
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("WAL");
        file.write_all(b"{}\n").expect("committed corruption");
        file.sync_all().expect("sync corruption fixture");

        assert!(LocalRuntimeHost::read_model_route_journal(&path).is_err());
    }

    #[test]
    fn legacy_model_route_snapshot_migrates_to_one_committed_wal_record() {
        let root = tempfile::tempdir().expect("state");
        let path = root.path().join("route.json");
        let mut legacy = journal();
        legacy.store_version = 2;
        std::fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let migrated = LocalRuntimeHost::read_model_route_journal(&path).expect("migration");

        assert_eq!(migrated.store_version, LOCAL_MODEL_ROUTE_STORE_VERSION);
        let records = LocalRuntimeHost::read_model_route_wal_records(&path).expect("WAL");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].revision, 1);
        assert_eq!(records[0].journal, migrated);
    }

    #[test]
    fn model_route_wal_rejects_a_revision_gap_and_identity_change() {
        let root = tempfile::tempdir().expect("state");
        let path = root.path().join("route.json");
        let first = journal();
        LocalRuntimeHost::persist_model_route_journal(&path, &first).expect("first commit");
        let gap = LocalModelRouteWalRecord {
            record_version: LOCAL_MODEL_ROUTE_WAL_RECORD_VERSION,
            revision: 3,
            journal: first.clone(),
        };
        let line = LocalRuntimeHost::encode_model_route_wal_record(&gap).expect("gap fixture");
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("WAL");
        file.write_all(&line).expect("append gap");
        file.sync_all().expect("sync gap");
        assert!(LocalRuntimeHost::read_model_route_journal(&path).is_err());

        let other_path = root.path().join("other-route.json");
        LocalRuntimeHost::persist_model_route_journal(&other_path, &first).expect("first commit");
        let mut wrong_identity = first;
        wrong_identity.run_id = Uuid::now_v7();
        assert!(
            LocalRuntimeHost::persist_model_route_journal(&other_path, &wrong_identity).is_err()
        );
        assert_eq!(
            LocalRuntimeHost::read_model_route_wal_records(&other_path)
                .expect("unchanged WAL")
                .len(),
            1
        );
    }

    #[test]
    fn model_route_wal_rejects_a_well_formed_state_rollback() {
        let root = tempfile::tempdir().expect("state");
        let path = root.path().join("route.json");
        let mut first = journal();
        first.same_provider_attempts = 1;
        first.inflight_provider_id = Some("primary".into());
        LocalRuntimeHost::persist_model_route_journal(&path, &first).expect("first commit");
        let mut rolled_back = first.clone();
        rolled_back.same_provider_attempts = 0;
        rolled_back.inflight_provider_id = None;
        assert!(rolled_back.is_well_formed());
        let record = LocalModelRouteWalRecord {
            record_version: LOCAL_MODEL_ROUTE_WAL_RECORD_VERSION,
            revision: 2,
            journal: rolled_back,
        };
        let line = LocalRuntimeHost::encode_model_route_wal_record(&record).expect("fixture");
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("WAL");
        file.write_all(&line).expect("append rollback");
        file.sync_all().expect("sync rollback fixture");

        assert!(LocalRuntimeHost::read_model_route_journal(&path).is_err());
    }

    #[test]
    fn model_route_wal_rejects_an_oversized_file_before_reading_it() {
        let root = tempfile::tempdir().expect("state");
        let path = root.path().join("route.json");
        let file = std::fs::File::create(&path).expect("WAL");
        file.set_len(LOCAL_MODEL_ROUTE_WAL_MAX_FILE_BYTES + 1)
            .expect("oversized sparse WAL");

        assert!(LocalRuntimeHost::read_model_route_journal(&path).is_err());
    }

    #[test]
    fn model_route_wal_compacts_before_exceeding_its_record_bound() {
        let root = tempfile::tempdir().expect("state");
        let path = root.path().join("route.json");
        let mut current = journal();
        for revision in 1..=LOCAL_MODEL_ROUTE_WAL_MAX_RECORDS + 1 {
            current.attempt_id = Uuid::now_v7();
            LocalRuntimeHost::persist_model_route_journal(&path, &current)
                .unwrap_or_else(|error| panic!("revision {revision}: {error}"));
        }

        let records = LocalRuntimeHost::read_model_route_wal_records(&path).expect("compacted WAL");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].revision, 1);
        assert_eq!(records[0].journal, current);
    }
}

struct DurationDeadlineGuard {
    stop: CancellationToken,
}

impl Drop for DurationDeadlineGuard {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl LocalRuntimeHost {
    pub fn start(config: LocalRuntimeConfig) -> Result<Self, LocalRuntimeError> {
        Self::start_for_invocation(config, local_invocation_context())
    }

    pub fn start_for_invocation(
        config: LocalRuntimeConfig,
        invocation: RuntimeInvocationContext,
    ) -> Result<Self, LocalRuntimeError> {
        Self::start_for_invocation_with_cancellation(config, invocation, CancellationToken::new())
    }

    pub fn start_with_cancellation(
        config: LocalRuntimeConfig,
        cancellation: CancellationToken,
    ) -> Result<Self, LocalRuntimeError> {
        Self::start_for_invocation_with_cancellation(
            config,
            local_invocation_context(),
            cancellation,
        )
    }

    pub fn start_for_invocation_with_cancellation(
        config: LocalRuntimeConfig,
        invocation: RuntimeInvocationContext,
        cancellation: CancellationToken,
    ) -> Result<Self, LocalRuntimeError> {
        // The caller may share its token with sibling components. The Host
        // owns only this descendant cancellation domain, which can therefore
        // be cancelled safely from `Drop` without cancelling the caller.
        let cancellation = cancellation.child_token();
        invocation
            .validate()
            .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;
        if config.agent_instructions.trim().is_empty() {
            return Err(LocalRuntimeError::Configuration(
                "agent instructions must not be blank".into(),
            ));
        }
        if !config.runtime_policy.is_bounded_and_safe() {
            return Err(LocalRuntimeError::Configuration(
                "runtime execution policy is invalid".into(),
            ));
        }
        if !config.workspace_root.is_absolute() || !config.workspace_root.is_dir() {
            return Err(LocalRuntimeError::Configuration(
                "workspace root must be an existing absolute directory".into(),
            ));
        }
        // A host that cannot checkpoint cannot resume, and would lose work on
        // exit without ever saying so.
        std::fs::create_dir_all(config.state_root.join("runs"))
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;

        if config.model_routing.candidates.is_empty()
            || config.model_routing.candidates.len() > 8
            || config.model_routing.allowed_regions.is_empty()
            || !config.model_routing.health_policy.is_bounded_and_safe()
        {
            return Err(LocalRuntimeError::Configuration(
                "local model routing candidates, regions or health policy are invalid".into(),
            ));
        }
        let mut provider_ids = BTreeSet::new();
        let mut model_routes = Vec::with_capacity(config.model_routing.candidates.len());
        let mut binding_candidates = Vec::with_capacity(config.model_routing.candidates.len());
        for candidate in &config.model_routing.candidates {
            if candidate.id.trim().is_empty()
                || candidate.id.len() > 128
                || !provider_ids.insert(candidate.id.clone())
                || candidate.region.trim().is_empty()
                || candidate.accepted_data_classes.is_empty()
                || !candidate.capabilities.contains(&Capability::Text)
                || !(1..=600_000).contains(&candidate.response_timeout_ms)
                || !(1..=600_000).contains(&candidate.stream_idle_timeout_ms)
            {
                return Err(LocalRuntimeError::Configuration(
                    "local Provider ids, regions and capabilities are invalid".into(),
                ));
            }
            let credential = ProviderCredential::bearer(candidate.api_key.clone())
                .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;
            let pricing = ProviderPricing {
                input_million_tokens_micros: candidate.cost_per_million_tokens_micros,
                output_million_tokens_micros: candidate.cost_per_million_tokens_micros,
            };
            let adapter = match candidate.protocol {
                ProviderProtocol::OpenAiCompatible => ProviderAdapter::from(
                    OpenAiCompatibleAdapter::new(OpenAiCompatibleConfig {
                        endpoint: candidate.endpoint.clone(),
                        model: candidate.model.clone(),
                        pricing,
                        response_timeout: Duration::from_millis(candidate.response_timeout_ms),
                        stream_idle_timeout: Duration::from_millis(
                            candidate.stream_idle_timeout_ms,
                        ),
                    })
                    .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?,
                ),
                ProviderProtocol::OpenAiResponses => ProviderAdapter::from(
                    OpenAiResponsesAdapter::new(OpenAiResponsesConfig {
                        endpoint: candidate.endpoint.clone(),
                        model: candidate.model.clone(),
                        pricing,
                        response_timeout: Duration::from_millis(candidate.response_timeout_ms),
                        stream_idle_timeout: Duration::from_millis(
                            candidate.stream_idle_timeout_ms,
                        ),
                    })
                    .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?,
                ),
                ProviderProtocol::AnthropicMessages => ProviderAdapter::from(
                    AnthropicMessagesAdapter::new(AnthropicMessagesConfig {
                        endpoint: candidate.endpoint.clone(),
                        model: candidate.model.clone(),
                        anthropic_version: "2023-06-01".into(),
                        pricing,
                        response_timeout: Duration::from_millis(candidate.response_timeout_ms),
                        stream_idle_timeout: Duration::from_millis(
                            candidate.stream_idle_timeout_ms,
                        ),
                    })
                    .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?,
                ),
            };
            let model_candidate = ModelCandidate {
                id: candidate.id.clone(),
                region: candidate.region.clone(),
                accepted_data_classes: candidate.accepted_data_classes.clone(),
                capabilities: candidate.capabilities.clone(),
                healthy: candidate.healthy,
                latency_ms: candidate.latency_ms,
                cost_per_million_tokens_micros: candidate.cost_per_million_tokens_micros,
            };
            binding_candidates.push(serde_json::json!({
                "id": candidate.id,
                "protocol": candidate.protocol,
                "endpoint": candidate.endpoint,
                "model": candidate.model,
                "region": candidate.region,
                "accepted_data_classes": candidate.accepted_data_classes,
                "capabilities": candidate.capabilities,
                "healthy": candidate.healthy,
                "latency_ms": candidate.latency_ms,
                "cost_per_million_tokens_micros": candidate.cost_per_million_tokens_micros,
                "response_timeout_ms": candidate.response_timeout_ms,
                "stream_idle_timeout_ms": candidate.stream_idle_timeout_ms,
            }));
            model_routes.push(LocalModelRoute {
                candidate: model_candidate,
                protocol: candidate.protocol,
                endpoint: candidate.endpoint.clone(),
                model: candidate.model.clone(),
                adapter,
                credential,
            });
        }
        let routing_constraints = RoutingConstraints {
            allowed_regions: config.model_routing.allowed_regions.clone(),
            data_class: config.model_routing.data_class,
            required_capabilities: BTreeSet::new(),
            max_cost_per_million_tokens_micros: config
                .model_routing
                .max_cost_per_million_tokens_micros,
        };
        let model_route_binding_digest = hex::encode(Sha256::digest(
            serde_json::to_vec(&serde_json::json!({
                "candidates": binding_candidates,
                "allowed_regions": routing_constraints.allowed_regions,
                "data_class": routing_constraints.data_class,
                "max_cost_per_million_tokens_micros": routing_constraints.max_cost_per_million_tokens_micros,
                "health_policy": config.model_routing.health_policy,
            }))
            .expect("local model routing config is serializable"),
        ));
        let provider_health = shared_local_provider_health(&config.state_root)?;
        let (mcp_client, stdio_mcp) = if config.mcp_servers.is_empty() {
            (None, None)
        } else {
            let http = DirectMcpFederationClient::for_open_servers(
                Duration::from_millis(config.runtime_policy.tool_execution.timeout_ms),
                true,
            )
            .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;
            let mut transports = HashMap::new();
            let mut stdio_configs = HashMap::new();
            let mut names = BTreeSet::new();
            for server in &config.mcp_servers {
                if server.server_id.is_nil()
                    || server.name.trim().is_empty()
                    || !names.insert(server.name.clone())
                    || server.tool_effect_overrides.len() > server.tool_names.len()
                    || server
                        .tool_effect_overrides
                        .keys()
                        .any(|tool_name| !server.tool_names.contains(tool_name))
                    || transports
                        .insert(server.server_id, server.transport.clone())
                        .is_some()
                {
                    return Err(LocalRuntimeError::Configuration(
                        "local MCP server ids, names, and effect overrides must be valid and authorized"
                            .into(),
                    ));
                }
                let stdio_process = match &server.transport {
                    LocalMcpTransportConfig::Stdio {
                        command,
                        args,
                        env,
                        cwd,
                    }
                    | LocalMcpTransportConfig::StdioV20250326 {
                        command,
                        args,
                        env,
                        cwd,
                    }
                    | LocalMcpTransportConfig::Stdio2026 {
                        command,
                        args,
                        env,
                        cwd,
                        ..
                    } => Some((command, args, env, cwd)),
                    _ => None,
                };
                if let Some((command, args, env, cwd)) = stdio_process {
                    if !command.is_absolute() || !command.is_file() {
                        return Err(LocalRuntimeError::Configuration(format!(
                            "stdio MCP command must be an existing absolute file: {}",
                            command.display()
                        )));
                    }
                    if args.len() > 128
                        || args.iter().any(|arg| arg.len() > 16 * 1024)
                        || env.len() > 128
                        || env.iter().any(|(name, value)| {
                            name.is_empty()
                                || name.contains('=')
                                || name.len() > 256
                                || value.len() > 64 * 1024
                        })
                        || cwd
                            .as_ref()
                            .is_some_and(|path| !path.is_absolute() || !path.is_dir())
                    {
                        return Err(LocalRuntimeError::Configuration(format!(
                            "stdio MCP process configuration is invalid for {}",
                            server.name
                        )));
                    }
                    stdio_configs.insert(
                        server.server_id,
                        LocalStdioMcpConfig {
                            command: command.clone(),
                            args: args.clone(),
                            env: env.clone(),
                            cwd: cwd.clone(),
                            protocol_revision: server.transport.protocol_revision(),
                            client_capabilities: server.transport.client_capabilities(),
                        },
                    );
                }
            }
            let required_session_capacity = stdio_configs.len().min(usize::from(
                config.runtime_policy.mcp_discovery.max_concurrent_servers,
            ));
            if config.mcp_lifecycle.max_sessions == 0
                || config.mcp_lifecycle.max_sessions > 64
                || config.mcp_lifecycle.max_sessions < required_session_capacity
                || config.mcp_lifecycle.session_idle_ttl.is_zero()
                    != config.mcp_lifecycle.sweep_interval.is_zero()
            {
                return Err(LocalRuntimeError::Configuration(
                    "local MCP lifecycle limits are invalid".into(),
                ));
            }
            let stdio = StdioMcpClient::new(
                stdio_configs,
                Duration::from_millis(config.runtime_policy.tool_execution.timeout_ms),
                config.mcp_lifecycle.clone(),
            );
            (
                Some(McpFederationClient::from_backend(LocalMcpBackend {
                    http,
                    transports: Arc::new(transports),
                    stdio: stdio.clone(),
                })),
                Some(stdio),
            )
        };

        let worker_id = Uuid::now_v7();
        let mut processor = WorkerProcessor::new(
            worker_id,
            vec![agent_protocol::Placement::Edge],
            1,
            env!("CARGO_PKG_VERSION").to_string(),
        )
        .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;
        let local_skill_key = SigningKey::from_bytes(&LOCAL_SKILL_SIGNING_KEY);
        processor.set_skill_artifact_verifier(agent_runtime_worker::SkillArtifactVerifier::new(
            LOCAL_SKILL_KEY_ID,
            local_skill_key.verifying_key(),
        ));

        let mut executors = std::collections::HashMap::<String, Arc<dyn ToolExecutor>>::new();
        let mut process_session_manager = None;
        if let Some(binary) = &config.trusted_workspace_tool {
            let trusted_root = binary.parent().ok_or_else(|| {
                LocalRuntimeError::Configuration(
                    "trusted workspace tool must have a parent directory".into(),
                )
            })?;
            // One executor per Tool rather than one shared read-write executor:
            // the read Tool then runs under a profile that grants no writes at
            // all, so a defect in it cannot change anything.
            for (name, access, effect, scope, description) in [
                (
                    WORKSPACE_READ_TOOL,
                    WorkspaceAccess::ReadOnly,
                    ToolEffect::Pure,
                    WORKSPACE_READ_SCOPE,
                    "Read one bounded UTF-8 text file from the local workspace",
                ),
                (
                    WORKSPACE_WRITE_TOOL,
                    WorkspaceAccess::ReadWrite,
                    ToolEffect::NonIdempotent,
                    WORKSPACE_WRITE_SCOPE,
                    "Write one bounded UTF-8 text file into the local workspace",
                ),
                (
                    SHELL_TOOL,
                    WorkspaceAccess::ReadWrite,
                    ToolEffect::NonIdempotent,
                    SHELL_SCOPE,
                    "Run one bounded shell command inside the local workspace",
                ),
            ] {
                let native = TrustedNativeExecutor::new(TrustedNativeToolDefinition {
                    trusted_root: trusted_root.to_path_buf(),
                    executable: binary.clone(),
                    fixed_args: vec!["--stdio".into()],
                    workspace_access: access,
                    max_stdout_bytes: 128 * 1024,
                    max_stderr_bytes: 16 * 1024,
                })
                .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;
                processor
                    .register_tool(WorkerToolDefinition {
                        descriptor: ToolDescriptor {
                            name: name.into(),
                            effect,
                            approval: ApprovalMode::Ask,
                            sandbox: SandboxClass::TrustedNative,
                            implementation_digest: native.implementation_digest().to_owned(),
                            required_scopes: BTreeSet::from([scope.to_owned()]),
                        },
                        description: description.into(),
                        // Keyed on the Tool, not on its Workspace access: shell
                        // is also ReadWrite but takes a command, and matching on
                        // access would have handed it the file schema.
                        input_schema: match name {
                            WORKSPACE_READ_TOOL => serde_json::json!({
                                "type": "object",
                                "properties": {"path": {"type": "string"}},
                                "required": ["path"],
                                "additionalProperties": false
                            }),
                            SHELL_TOOL => serde_json::json!({
                                "type": "object",
                                "properties": {"command": {"type": "string"}},
                                "required": ["command"],
                                "additionalProperties": false
                            }),
                            _ => serde_json::json!({
                                "type": "object",
                                "properties": {
                                    "path": {"type": "string"},
                                    "text": {"type": "string"}
                                },
                                "required": ["path", "text"],
                                "additionalProperties": false
                            }),
                        },
                    })
                    .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;
                executors.insert(name.to_owned(), Arc::new(native));
            }
        }
        if let Some(process) = &config.process_session {
            if process.fixed_args.len() > 128
                || process.fixed_args.iter().any(|arg| arg.len() > 16 * 1024)
            {
                return Err(LocalRuntimeError::Configuration(
                    "persistent process fixed arguments are invalid".into(),
                ));
            }
            let trusted_root = process.executable.parent().ok_or_else(|| {
                LocalRuntimeError::Configuration(
                    "persistent process executable must have a parent directory".into(),
                )
            })?;
            let native = TrustedNativeExecutor::new(TrustedNativeToolDefinition {
                trusted_root: trusted_root.to_path_buf(),
                executable: process.executable.clone(),
                fixed_args: process.fixed_args.clone(),
                workspace_access: WorkspaceAccess::ReadWrite,
                max_stdout_bytes: 1024 * 1024,
                max_stderr_bytes: 1024 * 1024,
            })
            .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;
            let manager = Arc::new(
                PersistentProcessSessionManager::new_with_governance_and_pty_supervisor(
                    config.state_root.join("tool-process-session-state"),
                    native,
                    process.max_output_chunk_bytes,
                    process.governance.clone(),
                    process.pty_supervisor.clone(),
                )
                .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?,
            );
            process_session_manager = Some(manager.clone());
            for (operation, effect, description) in [
                (
                    ProcessSessionToolOperation::Start,
                    ToolEffect::NonIdempotent,
                    "Start one durable interactive process session",
                ),
                (
                    ProcessSessionToolOperation::Write,
                    ToolEffect::NonIdempotent,
                    "Write bounded UTF-8 input to a durable process session",
                ),
                (
                    ProcessSessionToolOperation::Resize,
                    ToolEffect::Idempotent,
                    "Resize a durable PTY in character cells",
                ),
                (
                    ProcessSessionToolOperation::Poll,
                    ToolEffect::Pure,
                    "Read process output from explicit byte cursors",
                ),
                (
                    ProcessSessionToolOperation::Attach,
                    ToolEffect::Pure,
                    "Attach to the bounded tail of durable process output",
                ),
                (
                    ProcessSessionToolOperation::Wait,
                    ToolEffect::Pure,
                    "Wait for bounded process output or terminal state",
                ),
                (
                    ProcessSessionToolOperation::Interrupt,
                    ToolEffect::NonIdempotent,
                    "Interrupt the registered process group",
                ),
                (
                    ProcessSessionToolOperation::Close,
                    ToolEffect::NonIdempotent,
                    "Close and reap the registered process group",
                ),
            ] {
                let executor =
                    Arc::new(ProcessSessionToolExecutor::new(manager.clone(), operation));
                let name = operation.tool_name();
                let cursor_properties = serde_json::json!({
                    "session_id": {"type": "string", "format": "uuid"},
                    "stdout_cursor": {"type": "integer", "minimum": 0},
                    "stderr_cursor": {"type": "integer", "minimum": 0}
                });
                let input_schema = match operation {
                    ProcessSessionToolOperation::Start => serde_json::json!({
                        "type": "object",
                        "properties": {
                            "initial_stdin": {"type": "string", "maxLength": 65536},
                            "yield_time_ms": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": config.runtime_policy.tool_execution.timeout_ms.min(300000)
                            },
                            "tty": {"type": "boolean"},
                            "cols": {"type": "integer", "minimum": 1, "maximum": 2000},
                            "rows": {"type": "integer", "minimum": 1, "maximum": 2000}
                        },
                        "additionalProperties": false
                    }),
                    ProcessSessionToolOperation::Write => serde_json::json!({
                        "type": "object",
                        "properties": {
                            "session_id": {"type": "string", "format": "uuid"},
                            "stdout_cursor": {"type": "integer", "minimum": 0},
                            "stderr_cursor": {"type": "integer", "minimum": 0},
                            "stdin": {"type": "string", "minLength": 1, "maxLength": 65536},
                            "yield_time_ms": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": config.runtime_policy.tool_execution.timeout_ms.min(300000)
                            }
                        },
                        "required": ["session_id", "stdin"],
                        "additionalProperties": false
                    }),
                    ProcessSessionToolOperation::Resize => serde_json::json!({
                        "type": "object",
                        "properties": {
                            "session_id": {"type": "string", "format": "uuid"},
                            "stdout_cursor": {"type": "integer", "minimum": 0},
                            "stderr_cursor": {"type": "integer", "minimum": 0},
                            "cols": {"type": "integer", "minimum": 1, "maximum": 2000},
                            "rows": {"type": "integer", "minimum": 1, "maximum": 2000}
                        },
                        "required": ["session_id", "cols", "rows"],
                        "additionalProperties": false
                    }),
                    ProcessSessionToolOperation::Attach => serde_json::json!({
                        "type": "object",
                        "properties": {
                            "session_id": {"type": "string", "format": "uuid"},
                            "max_bytes": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": process.max_output_chunk_bytes
                            }
                        },
                        "required": ["session_id", "max_bytes"],
                        "additionalProperties": false
                    }),
                    ProcessSessionToolOperation::Wait => serde_json::json!({
                        "type": "object",
                        "properties": {
                            "session_id": {"type": "string", "format": "uuid"},
                            "stdout_cursor": {"type": "integer", "minimum": 0},
                            "stderr_cursor": {"type": "integer", "minimum": 0},
                            "yield_time_ms": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": config.runtime_policy.tool_execution.timeout_ms.min(300000)
                            }
                        },
                        "required": ["session_id", "yield_time_ms"],
                        "additionalProperties": false
                    }),
                    ProcessSessionToolOperation::Poll
                    | ProcessSessionToolOperation::Interrupt
                    | ProcessSessionToolOperation::Close => serde_json::json!({
                        "type": "object",
                        "properties": cursor_properties,
                        "required": ["session_id"],
                        "additionalProperties": false
                    }),
                };
                processor
                    .register_tool(WorkerToolDefinition {
                        descriptor: ToolDescriptor {
                            name: name.into(),
                            effect,
                            approval: ApprovalMode::Ask,
                            sandbox: SandboxClass::TrustedNative,
                            implementation_digest: executor.implementation_digest().to_owned(),
                            required_scopes: BTreeSet::from([PROCESS_SESSION_SCOPE.to_owned()]),
                        },
                        description: description.into(),
                        input_schema,
                    })
                    .map_err(|error| LocalRuntimeError::Configuration(error.to_string()))?;
                executors.insert(name.to_owned(), executor);
            }
        }

        Ok(Self {
            config,
            invocation,
            processor,
            model_routes,
            routing_constraints,
            model_route_binding_digest,
            provider_health,
            mcp_client,
            stdio_mcp,
            executors,
            process_session_manager,
            worker_id,
            cancellation,
            duration_expired: Arc::new(AtomicBool::new(false)),
            subagent_tasks: HashMap::new(),
            pending_mcp_input: None,
        })
    }

    /// Builds the same `RunExecutionCommand` contract the Java scheduler emits.
    /// Owner epoch, fencing token, and incarnation exist to arbitrate between
    /// competing Workers; single-writer local execution has nothing to
    /// arbitrate, so they take fixed local values.
    fn local_command(&self, run_id: Uuid, input: &str, owner_epoch: u64) -> RunExecutionCommand {
        self.local_command_with_lineage(
            run_id,
            input,
            owner_epoch,
            AgentLineage {
                root_run_id: run_id,
                parent_run_id: None,
                delegation_id: None,
                depth: 0,
                role: "primary".into(),
            },
        )
    }

    fn local_command_with_lineage(
        &self,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
        lineage: AgentLineage,
    ) -> RunExecutionCommand {
        self.local_command_with_lineage_and_history(run_id, input, owner_epoch, lineage, Vec::new())
    }

    fn local_command_with_lineage_and_history(
        &self,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
        lineage: AgentLineage,
        subagent_history: Vec<SubagentConversationTurn>,
    ) -> RunExecutionCommand {
        self.local_command_with_context(run_id, input, owner_epoch, lineage, subagent_history, None)
    }

    fn local_command_with_context(
        &self,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
        lineage: AgentLineage,
        subagent_history: Vec<SubagentConversationTurn>,
        history_import: Option<HistoryImport>,
    ) -> RunExecutionCommand {
        self.local_command_with_session_context(
            run_id,
            run_id,
            input,
            owner_epoch,
            lineage,
            subagent_history,
            history_import,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn local_command_with_session_context(
        &self,
        run_id: Uuid,
        session_id: Uuid,
        input: &str,
        owner_epoch: u64,
        lineage: AgentLineage,
        subagent_history: Vec<SubagentConversationTurn>,
        history_import: Option<HistoryImport>,
        session_branch: Option<SessionBranchSnapshot>,
    ) -> RunExecutionCommand {
        let issued_at = Utc::now();
        // A one-shot root still owns a deterministic empty branch. Binding it
        // lets the current execution contract preserve ordered Tool batches
        // across restart without pretending imported lower-authority history
        // belongs to an authoritative Session.
        let session_branch = if lineage.depth == 0 && history_import.is_none() {
            Some(
                session_branch.unwrap_or_else(|| SessionBranchSnapshot::new(run_id, 1, Vec::new())),
            )
        } else {
            session_branch
        };
        let execution_schema_version = if session_branch.is_some() {
            RUN_EXECUTION_SCHEMA_VERSION
        } else if lineage.depth == 0 {
            // Legacy standalone Run and explicit history-import APIs have no
            // authoritative Session head. They remain v15 instead of claiming
            // the root Session guarantee introduced by v16.
            15
        } else if subagent_history
            .iter()
            .any(|turn| turn.result.transcript.is_empty())
        {
            // A pre-v14 checkpoint can preserve its legacy text-only history,
            // but it must not claim the typed transcript guarantee introduced
            // by v14. New turns always produce v3 transcript-bound receipts.
            13
        } else {
            RUN_EXECUTION_SCHEMA_VERSION
        };
        let model_policy_snapshot = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "routing": "ranked_frozen_failover",
            "binding_digest": self.model_route_binding_digest,
            "health_policy": self.config.model_routing.health_policy,
            "candidates": self.model_routes.iter().map(|route| serde_json::json!({
                "provider": route.candidate.id,
                "protocol": route.protocol,
                "endpoint": route.endpoint,
                "model": route.model,
                "region": route.candidate.region,
                "accepted_data_classes": route.candidate.accepted_data_classes,
                "capabilities": route.candidate.capabilities,
                "healthy": route.candidate.healthy,
                "latency_ms": route.candidate.latency_ms,
                "cost_per_million_tokens_micros": route.candidate.cost_per_million_tokens_micros,
                "response_timeout_ms": self.config.model_routing.candidates.iter()
                    .find(|candidate| candidate.id == route.candidate.id)
                    .map(|candidate| candidate.response_timeout_ms),
                "stream_idle_timeout_ms": self.config.model_routing.candidates.iter()
                    .find(|candidate| candidate.id == route.candidate.id)
                    .map(|candidate| candidate.stream_idle_timeout_ms),
            })).collect::<Vec<_>>()
        }))
        .expect("local model policy snapshot is serializable");
        let mut runtime_policy = self.config.runtime_policy.clone();
        if execution_schema_version < 17 {
            runtime_policy.schema_version = 3;
            runtime_policy.tool_execution.max_concurrent_tools = 1;
        }
        RunExecutionCommand {
            schema_version: execution_schema_version,
            message_id: Uuid::now_v7(),
            tenant_id: self.invocation.tenant_id,
            application_id: self.invocation.application_id,
            workload_identity_id: self.invocation.workload_identity_id,
            run_id,
            session_id,
            workspace_id: self.invocation.workspace_id,
            agent_version_id: self.invocation.agent_version_id,
            model_policy_id: self.invocation.model_policy_id,
            attempt_id: Uuid::now_v7(),
            worker_id: self.worker_id,
            worker_incarnation_id: self.worker_id,
            owner_epoch,
            fencing_token: Uuid::now_v7(),
            issued_at,
            lease_expires_at: issued_at + ChronoDuration::seconds(3600),
            // Local mode crosses no process boundary to reach the provider, so
            // there is no identity to present; this placeholder is never sent.
            workload_token: serde_json::from_value(serde_json::json!("local.local.local"))
                .expect("local workload token placeholder is well formed"),
            delegated_scopes: self.config.delegated_scopes.clone(),
            agent_instructions: self.config.agent_instructions.clone(),
            model_policy_snapshot_base64: base64::engine::general_purpose::STANDARD
                .encode(&model_policy_snapshot),
            model_policy_digest: hex::encode(Sha256::digest(&model_policy_snapshot)),
            skill_snapshots: vec![local_skill_snapshot(
                self.invocation,
                self.config.trusted_workspace_tool.is_some(),
                self.config.process_session.is_some(),
                &self.config.mcp_servers,
            )],
            lineage,
            subagent_roles: self.config.subagent_roles.clone(),
            // Empty: the local operator has no way to configure this yet, and an
            // empty map means every Tool asks. When the local host grows a
            // policy setting it belongs in LocalRuntimeConfig, so the source is
            // still configuration rather than a constant in the code path.
            tool_approval_policies: Default::default(),
            // Local mode accepts only open endpoints. Credential envelopes are
            // a cloud boundary and are rejected by the local MCP backend rather
            // than opened inside the Agent process.
            mcp_servers: self
                .config
                .mcp_servers
                .iter()
                .map(|server| agent_protocol::McpServerSnapshot {
                    server_id: server.server_id,
                    name: server.name.clone(),
                    endpoint: server.transport.binding_endpoint(),
                    credential_envelope_base64: String::new(),
                    oauth_credential_id: None,
                    required: server.required,
                    tool_effect_overrides: server.tool_effect_overrides.clone(),
                    protocol_revision: server.transport.protocol_revision(),
                    client_capabilities: server.transport.client_capabilities(),
                })
                .collect(),
            runtime_policy: Some(runtime_policy),
            subagent_history,
            history_import,
            session_branch,
            input: input.to_owned(),
            budget: self.config.budget.clone(),
        }
    }

    fn run_dir(&self, run_id: Uuid) -> PathBuf {
        self.config.state_root.join("runs").join(run_id.to_string())
    }

    fn required_model_capabilities(request: &agent_protocol::ModelRequest) -> BTreeSet<Capability> {
        let mut required = BTreeSet::from([Capability::Text]);
        if !request.tools.is_empty()
            || request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|part| {
                    matches!(
                        part,
                        ProtocolContentPart::ToolCall { .. }
                            | ProtocolContentPart::ToolResult { .. }
                    )
                })
        {
            required.insert(Capability::ToolUse);
        }
        if request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|part| matches!(part, ProtocolContentPart::Image { .. }))
        {
            required.insert(Capability::Vision);
        }
        if request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|part| matches!(part, ProtocolContentPart::Audio { .. }))
        {
            required.insert(Capability::Audio);
        }
        if request.output_schema.is_some() {
            required.insert(Capability::StructuredOutput);
        }
        required
    }

    fn frozen_model_routes(
        &self,
        request: &agent_protocol::ModelRequest,
        invocation_digest: &str,
    ) -> Result<Vec<LocalModelRoute>, LocalRuntimeError> {
        let candidates = self
            .model_routes
            .iter()
            .map(|route| route.candidate.clone())
            .collect::<Vec<_>>();
        let mut constraints = self.routing_constraints.clone();
        constraints.required_capabilities = Self::required_model_capabilities(request);
        let maximum = usize::from(
            self.config
                .runtime_policy
                .model_failover
                .max_provider_attempts,
        );
        let health = self.provider_health.lock().map_err(|_| {
            LocalRuntimeError::StateRoot("Provider health state is poisoned".into())
        })?;
        let ranked = rank_candidates(&candidates, &constraints)
            .into_iter()
            .filter(|candidate| {
                health.is_eligible_for_new_route(
                    &self.model_route_binding_digest,
                    &candidate.id,
                    invocation_digest,
                    &self.config.model_routing.health_policy,
                )
            })
            .take(maximum)
            .map(|candidate| {
                self.model_routes
                    .iter()
                    .find(|route| route.candidate.id == candidate.id)
                    .cloned()
                    .expect("ranked candidate came from the local route set")
            })
            .collect::<Vec<_>>();
        if ranked.is_empty() {
            return Err(LocalRuntimeError::ProviderSelection(
                "no healthy local Provider satisfies the Run region, data, capability and cost constraints"
                    .into(),
            ));
        }
        Ok(ranked)
    }

    fn restored_model_routes(
        &self,
        candidate_ids: &[String],
    ) -> Result<Vec<LocalModelRoute>, LocalRuntimeError> {
        candidate_ids
            .iter()
            .map(|candidate_id| {
                self.model_routes
                    .iter()
                    .find(|route| route.candidate.id == *candidate_id)
                    .cloned()
                    .ok_or_else(|| {
                        LocalRuntimeError::Checkpoint(
                            "frozen local model route references an unknown Provider".into(),
                        )
                    })
            })
            .collect()
    }

    fn model_route_invocation_digest(&self, request: &agent_protocol::ModelRequest) -> String {
        let material = serde_json::to_vec(&serde_json::json!({
            "request": request,
            "route_binding_digest": self.model_route_binding_digest,
            "failover_policy": self.config.runtime_policy.model_failover,
        }))
        .expect("provider-neutral model request is serializable");
        hex::encode(Sha256::digest(material))
    }

    fn model_route_journal_path(&self, run_id: Uuid, invocation_digest: &str) -> PathBuf {
        self.run_dir(run_id)
            .join("model-routes")
            .join(format!("{invocation_digest}.json"))
    }

    fn persist_model_route_journal(
        path: &Path,
        journal: &LocalModelRouteJournal,
    ) -> Result<(), LocalRuntimeError> {
        if journal.store_version != LOCAL_MODEL_ROUTE_STORE_VERSION || !journal.is_well_formed() {
            return Err(LocalRuntimeError::Checkpoint(
                "refusing to persist malformed local model route journal".into(),
            ));
        }
        let records = if Self::model_route_journal_exists(path)? {
            Self::read_model_route_wal_records(path)?
        } else {
            Vec::new()
        };
        if let Some(previous) = records.last() {
            if previous.journal == *journal {
                let mut file = std::fs::OpenOptions::new()
                    .read(true)
                    .append(true)
                    .open(path)
                    .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
                Self::repair_uncommitted_event_tail(&mut file)?;
                return Ok(());
            }
            if previous.journal.completed {
                return Err(LocalRuntimeError::Checkpoint(
                    "refusing to append after a completed local model route WAL".into(),
                ));
            }
            if journal.run_id != previous.journal.run_id
                || journal.invocation_digest != previous.journal.invocation_digest
                || journal.model_route_binding_digest != previous.journal.model_route_binding_digest
                || journal.candidate_ids != previous.journal.candidate_ids
            {
                return Err(LocalRuntimeError::Checkpoint(
                    "refusing to change local model route WAL immutable identity".into(),
                ));
            }
            let candidate = LocalModelRouteWalRecord {
                record_version: LOCAL_MODEL_ROUTE_WAL_RECORD_VERSION,
                revision: previous.revision.saturating_add(1),
                journal: journal.clone(),
            };
            if !candidate.follows(previous) {
                return Err(LocalRuntimeError::Checkpoint(
                    "refusing to roll back local model route WAL state".into(),
                ));
            }
        }
        let compact = records.len() >= LOCAL_MODEL_ROUTE_WAL_MAX_RECORDS;
        let revision = if compact {
            1
        } else {
            records
                .last()
                .map_or(1, |record| record.revision.saturating_add(1))
        };
        let record = LocalModelRouteWalRecord {
            record_version: LOCAL_MODEL_ROUTE_WAL_RECORD_VERSION,
            revision,
            journal: journal.clone(),
        };
        let line = Self::encode_model_route_wal_record(&record)?;
        if records.is_empty() || compact {
            return durable_file::replace(path, &line);
        }

        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        Self::repair_uncommitted_event_tail(&mut file)?;
        file.write_all(&line)
            .and_then(|()| file.sync_data())
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))
    }

    fn read_model_route_journal(path: &Path) -> Result<LocalModelRouteJournal, LocalRuntimeError> {
        Self::read_model_route_wal_records(path)?
            .last()
            .map(|record| record.journal.clone())
            .ok_or_else(|| {
                LocalRuntimeError::Checkpoint(
                    "local model route journal has no committed record".into(),
                )
            })
    }

    fn encode_model_route_wal_record(
        record: &LocalModelRouteWalRecord,
    ) -> Result<Vec<u8>, LocalRuntimeError> {
        if !record.is_well_formed() {
            return Err(LocalRuntimeError::Checkpoint(
                "refusing to encode malformed local model route WAL record".into(),
            ));
        }
        let mut line = serde_json::to_vec(record)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        line.push(b'\n');
        if line.len() > LOCAL_MODEL_ROUTE_WAL_MAX_LINE_BYTES {
            return Err(LocalRuntimeError::Checkpoint(format!(
                "local model route WAL record exceeds {LOCAL_MODEL_ROUTE_WAL_MAX_LINE_BYTES} bytes"
            )));
        }
        Ok(line)
    }

    fn read_model_route_wal_records(
        path: &Path,
    ) -> Result<Vec<LocalModelRouteWalRecord>, LocalRuntimeError> {
        let metadata = std::fs::metadata(path)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        if !metadata.is_file() {
            return Err(LocalRuntimeError::StateRoot(
                "local model route WAL is not a regular file".into(),
            ));
        }
        if metadata.len() > LOCAL_MODEL_ROUTE_WAL_MAX_FILE_BYTES {
            return Err(LocalRuntimeError::Checkpoint(format!(
                "local model route WAL exceeds {LOCAL_MODEL_ROUTE_WAL_MAX_FILE_BYTES} bytes"
            )));
        }
        let body =
            std::fs::read(path).map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;

        // V1/V2 used one pretty-printed JSON snapshot. Convert it in place to
        // the first committed WAL record before any Provider can be invoked.
        if let Ok(mut legacy) = serde_json::from_slice::<LocalModelRouteJournal>(&body) {
            if !matches!(legacy.store_version, 1 | 2) || !legacy.is_well_formed() {
                return Err(LocalRuntimeError::Checkpoint(
                    "legacy local model route journal is malformed".into(),
                ));
            }
            legacy.store_version = LOCAL_MODEL_ROUTE_STORE_VERSION;
            let record = LocalModelRouteWalRecord {
                record_version: LOCAL_MODEL_ROUTE_WAL_RECORD_VERSION,
                revision: 1,
                journal: legacy,
            };
            let line = Self::encode_model_route_wal_record(&record)?;
            durable_file::replace(path, &line)?;
            return Ok(vec![record]);
        }

        let committed_length = body
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        if committed_length == 0 {
            return Err(LocalRuntimeError::Checkpoint(
                "local model route WAL has no committed record".into(),
            ));
        }
        let mut records: Vec<LocalModelRouteWalRecord> = Vec::new();
        for line in body[..committed_length - 1].split(|byte| *byte == b'\n') {
            if line.is_empty() {
                return Err(LocalRuntimeError::Checkpoint(
                    "local model route WAL contains a committed empty record".into(),
                ));
            }
            if line.len() + 1 > LOCAL_MODEL_ROUTE_WAL_MAX_LINE_BYTES {
                return Err(LocalRuntimeError::Checkpoint(
                    "local model route WAL contains an oversized record".into(),
                ));
            }
            let record: LocalModelRouteWalRecord = serde_json::from_slice(line)
                .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
            let expected_revision = u64::try_from(records.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1);
            if !record.is_well_formed() || record.revision != expected_revision {
                return Err(LocalRuntimeError::Checkpoint(
                    "local model route WAL record is malformed or out of sequence".into(),
                ));
            }
            if let Some(previous) = records.last()
                && !record.follows(previous)
            {
                return Err(LocalRuntimeError::Checkpoint(
                    "local model route WAL changed immutable identity or rolled back state".into(),
                ));
            }
            records.push(record);
            if records.len() > LOCAL_MODEL_ROUTE_WAL_MAX_RECORDS {
                return Err(LocalRuntimeError::Checkpoint(
                    "local model route WAL exceeds its bounded record count".into(),
                ));
            }
        }
        Ok(records)
    }

    fn model_route_journal_exists(path: &Path) -> Result<bool, LocalRuntimeError> {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => Ok(true),
            Ok(_) => Err(LocalRuntimeError::StateRoot(
                "local model route journal path is not a regular file".into(),
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(LocalRuntimeError::StateRoot(error.to_string())),
        }
    }

    fn route_failure(provider_id: &str, error: &ProviderExecutionError) -> LocalModelRouteFailure {
        let (kind, retryable, status, retry_after_ms, message) = match error {
            ProviderExecutionError::Provider {
                kind,
                retryable,
                status,
                retry_after_ms,
                message,
            } => (
                *kind,
                *retryable,
                *status,
                *retry_after_ms,
                message.as_str(),
            ),
            ProviderExecutionError::Cancelled => (
                ModelErrorKind::Timeout,
                false,
                None,
                None,
                "provider call cancelled",
            ),
            ProviderExecutionError::ConsumerClosed => (
                ModelErrorKind::Unavailable,
                false,
                None,
                None,
                "provider event consumer closed",
            ),
            ProviderExecutionError::InvalidConfiguration(message) => (
                ModelErrorKind::Protocol,
                false,
                None,
                None,
                message.as_str(),
            ),
        };
        LocalModelRouteFailure {
            provider_id: provider_id.to_owned(),
            kind,
            retryable,
            status,
            retry_after_ms,
            message_digest: hex::encode(Sha256::digest(message.as_bytes())),
        }
    }

    fn can_fallback(&self, failure: &LocalModelRouteFailure, committed_events: usize) -> bool {
        committed_events == 0
            && failure.retryable
            && self
                .config
                .runtime_policy
                .model_failover
                .fallback_on
                .contains(&failure.kind)
    }

    fn same_provider_retry_delay_ms(
        &self,
        failure: &LocalModelRouteFailure,
        completed_attempts: u8,
    ) -> u64 {
        let policy = &self.config.model_routing.health_policy;
        let exponent = u32::from(completed_attempts.saturating_sub(1)).min(31);
        let backoff = policy
            .initial_retry_backoff_ms
            .saturating_mul(1_u64 << exponent)
            .min(policy.max_retry_backoff_ms);
        failure
            .retry_after_ms
            .map(|delay| delay.min(policy.max_retry_after_ms).max(backoff))
            .unwrap_or(backoff)
    }

    async fn wait_for_model_retry(
        &self,
        retry_not_before_unix_ms: i64,
    ) -> Result<(), LocalRuntimeError> {
        let remaining_ms = retry_not_before_unix_ms.saturating_sub(Utc::now().timestamp_millis());
        if remaining_ms <= 0 {
            return Ok(());
        }
        let delay = Duration::from_millis(u64::try_from(remaining_ms).unwrap_or(u64::MAX));
        tokio::select! {
            () = self.cancellation.cancelled() => Err(LocalRuntimeError::Provider(
                "model retry wait was cancelled".into(),
            )),
            () = tokio::time::sleep(delay) => Ok(()),
        }
    }

    fn flush_model_route_observations(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        path: &Path,
        journal: &mut LocalModelRouteJournal,
        event_types: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        while journal.reported_retry_count < journal.retry_attempts.len() {
            let retry = journal.retry_attempts[journal.reported_retry_count].clone();
            let event = self
                .processor
                .record_model_provider_retry_scheduled(
                    attempt_id,
                    &retry.provider_id,
                    retry.provider_attempt,
                    retry.delay_ms,
                    retry.failure.kind,
                    retry.failure.status,
                )
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            self.emit(run_id, &event, event_types)?;
            self.persist_checkpoint(run_id, attempt_id)?;
            journal.reported_retry_count += 1;
            Self::persist_model_route_journal(path, journal)?;
        }
        while journal.reported_failure_count < journal.failed_attempts.len() {
            let failure = journal.failed_attempts[journal.reported_failure_count].clone();
            let event = self
                .processor
                .record_model_provider_failure(
                    attempt_id,
                    &failure.provider_id,
                    failure.kind,
                    failure.retryable,
                    failure.status,
                )
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            self.emit(run_id, &event, event_types)?;
            self.persist_checkpoint(run_id, attempt_id)?;
            journal.reported_failure_count += 1;
            Self::persist_model_route_journal(path, journal)?;
        }
        if let Some(failure) = journal.terminal_failure.clone()
            && !journal.terminal_failure_reported
        {
            let event = self
                .processor
                .record_model_provider_failure(
                    attempt_id,
                    &failure.provider_id,
                    failure.kind,
                    failure.retryable,
                    failure.status,
                )
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            self.emit(run_id, &event, event_types)?;
            self.persist_checkpoint(run_id, attempt_id)?;
            journal.terminal_failure_reported = true;
            Self::persist_model_route_journal(path, journal)?;
        }
        if let Some(provider_id) = journal.selected_provider_id.clone()
            && !journal.selection_reported
        {
            let failed_provider_ids = journal
                .failed_attempts
                .iter()
                .map(|failure| failure.provider_id.clone())
                .collect::<Vec<_>>();
            let event = self
                .processor
                .record_model_provider_selection(attempt_id, &provider_id, &failed_provider_ids)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            self.emit(run_id, &event, event_types)?;
            self.persist_checkpoint(run_id, attempt_id)?;
            journal.selection_reported = true;
            Self::persist_model_route_journal(path, journal)?;
        }
        Ok(())
    }

    async fn execute_model_with_frozen_routing(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        request: &agent_protocol::ModelRequest,
        event_types: &mut Vec<String>,
    ) -> Result<(PathBuf, Vec<ModelStreamEvent>), LocalRuntimeError> {
        let invocation_digest = self.model_route_invocation_digest(request);
        let path = self.model_route_journal_path(run_id, &invocation_digest);
        let (routes, mut journal) = if Self::model_route_journal_exists(&path)? {
            let mut journal = Self::read_model_route_journal(&path)?;
            if journal.run_id != run_id
                || journal.invocation_digest != invocation_digest
                || journal.model_route_binding_digest != self.model_route_binding_digest
            {
                return Err(LocalRuntimeError::Checkpoint(
                    "local model route journal does not match the restored invocation".into(),
                ));
            }
            if journal.completed && journal.attempt_id != attempt_id {
                let archive = path.with_file_name(format!(
                    "{}.{}.completed.json",
                    journal.invocation_digest, journal.attempt_id
                ));
                if Self::model_route_journal_exists(&archive)? {
                    return Err(LocalRuntimeError::Checkpoint(
                        "completed local model route archive already exists".into(),
                    ));
                }
                durable_file::rename(&path, &archive)?;
                let routes = self.frozen_model_routes(request, &invocation_digest)?;
                let candidate_ids = routes
                    .iter()
                    .map(|route| route.candidate.id.clone())
                    .collect::<Vec<_>>();
                journal = LocalModelRouteJournal {
                    store_version: LOCAL_MODEL_ROUTE_STORE_VERSION,
                    run_id,
                    attempt_id,
                    invocation_digest: invocation_digest.clone(),
                    model_route_binding_digest: self.model_route_binding_digest.clone(),
                    candidate_ids: candidate_ids.clone(),
                    next_candidate_index: 0,
                    failed_attempts: Vec::new(),
                    reported_failure_count: 0,
                    retry_attempts: Vec::new(),
                    reported_retry_count: 0,
                    same_provider_attempts: 0,
                    inflight_provider_id: None,
                    retry_not_before_unix_ms: None,
                    selected_provider_id: None,
                    selection_reported: false,
                    terminal_failure: None,
                    terminal_failure_reported: false,
                    staged_events: Vec::new(),
                    completed: false,
                };
                (routes, journal)
            } else if journal.attempt_id != attempt_id {
                journal.attempt_id = attempt_id;
                let routes = self.restored_model_routes(&journal.candidate_ids)?;
                (routes, journal)
            } else {
                let routes = self.restored_model_routes(&journal.candidate_ids)?;
                (routes, journal)
            }
        } else {
            let routes = self.frozen_model_routes(request, &invocation_digest)?;
            let candidate_ids = routes
                .iter()
                .map(|route| route.candidate.id.clone())
                .collect::<Vec<_>>();
            let journal = LocalModelRouteJournal {
                store_version: LOCAL_MODEL_ROUTE_STORE_VERSION,
                run_id,
                attempt_id,
                invocation_digest: invocation_digest.clone(),
                model_route_binding_digest: self.model_route_binding_digest.clone(),
                candidate_ids,
                next_candidate_index: 0,
                failed_attempts: Vec::new(),
                reported_failure_count: 0,
                retry_attempts: Vec::new(),
                reported_retry_count: 0,
                same_provider_attempts: 0,
                inflight_provider_id: None,
                retry_not_before_unix_ms: None,
                selected_provider_id: None,
                selection_reported: false,
                terminal_failure: None,
                terminal_failure_reported: false,
                staged_events: Vec::new(),
                completed: false,
            };
            (routes, journal)
        };
        if let Some(provider_id) = journal.inflight_provider_id.take() {
            let failure = LocalModelRouteFailure {
                provider_id: provider_id.clone(),
                kind: ModelErrorKind::Unavailable,
                retryable: true,
                status: None,
                retry_after_ms: None,
                message_digest: hex::encode(Sha256::digest(
                    b"Provider invocation was interrupted before a durable response",
                )),
            };
            if journal.same_provider_attempts
                < self
                    .config
                    .model_routing
                    .health_policy
                    .max_same_provider_attempts
            {
                let delay_ms =
                    self.same_provider_retry_delay_ms(&failure, journal.same_provider_attempts);
                let retry_not_before_unix_ms = Utc::now()
                    .timestamp_millis()
                    .saturating_add(i64::try_from(delay_ms).unwrap_or(i64::MAX));
                journal.retry_attempts.push(LocalModelRouteRetry {
                    provider_id,
                    provider_attempt: journal.same_provider_attempts.saturating_add(1),
                    failure,
                    delay_ms,
                    retry_not_before_unix_ms,
                });
                journal.retry_not_before_unix_ms = Some(retry_not_before_unix_ms);
            } else if journal.next_candidate_index + 1 < routes.len() {
                journal.failed_attempts.push(failure);
                journal.next_candidate_index += 1;
                journal.same_provider_attempts = 0;
                journal.retry_not_before_unix_ms = None;
            } else {
                journal.terminal_failure = Some(failure);
                journal.terminal_failure_reported = false;
            }
            Self::persist_model_route_journal(&path, &journal)?;
        }
        self.flush_model_route_observations(run_id, attempt_id, &path, &mut journal, event_types)?;
        if !journal.staged_events.is_empty() {
            return Ok((path, journal.staged_events.clone()));
        }
        if journal.completed {
            return Err(LocalRuntimeError::Checkpoint(
                "completed local model route was requested again from an older execution state"
                    .into(),
            ));
        }
        if journal.terminal_failure.is_some() && journal.staged_events.is_empty() {
            // A known retryable, pre-output failure with no remaining
            // candidate leaves the Run recoverable. A replacement attempt may
            // retry that exact frozen candidate, but never a candidate that
            // already failed earlier in the chain.
            if journal.same_provider_attempts
                >= self
                    .config
                    .model_routing
                    .health_policy
                    .max_same_provider_attempts
            {
                let failure = journal
                    .terminal_failure
                    .as_ref()
                    .expect("terminal failure was checked")
                    .clone();
                journal.staged_events = vec![ModelStreamEvent::Failed {
                    kind: failure.kind,
                    retryable: false,
                    message: format!(
                        "Provider {} exhausted the frozen same-provider attempt budget; diagnostic digest {}",
                        failure.provider_id, failure.message_digest
                    ),
                }];
                Self::persist_model_route_journal(&path, &journal)?;
                return Ok((path, journal.staged_events.clone()));
            }
            journal.terminal_failure = None;
            journal.terminal_failure_reported = false;
        }

        loop {
            if let Some(retry_not_before) = journal.retry_not_before_unix_ms {
                self.wait_for_model_retry(retry_not_before).await?;
                journal.retry_not_before_unix_ms = None;
            }
            let route = routes
                .get(journal.next_candidate_index)
                .ok_or_else(|| {
                    LocalRuntimeError::Checkpoint(
                        "local model route cursor is outside the frozen chain".into(),
                    )
                })?
                .clone();
            let admission = self
                .provider_health
                .lock()
                .map_err(|_| {
                    LocalRuntimeError::StateRoot("Provider health state is poisoned".into())
                })?
                .admission(
                    &self.model_route_binding_digest,
                    &route.candidate.id,
                    &journal.invocation_digest,
                    &self.config.model_routing.health_policy,
                )?;
            if admission == LocalProviderAdmission::Skip {
                if journal.next_candidate_index + 1 < routes.len() {
                    journal.next_candidate_index += 1;
                    journal.same_provider_attempts = 0;
                    journal.retry_not_before_unix_ms = None;
                    continue;
                }
                return Err(LocalRuntimeError::Provider(
                    "all Providers in the frozen route are cooling down or already have a half-open probe"
                        .into(),
                ));
            }
            journal.same_provider_attempts = journal.same_provider_attempts.saturating_add(1);
            journal.inflight_provider_id = Some(route.candidate.id.clone());
            Self::persist_model_route_journal(&path, &journal)?;
            let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
            let cancellation = self.cancellation.child_token();
            let call = route.adapter.execute(
                &route.candidate.id,
                request,
                &route.credential,
                cancellation,
                sender,
            );
            let collector = async {
                let mut events = Vec::new();
                while let Some(event) = receiver.recv().await {
                    events.push(event);
                }
                events
            };
            let (result, mut events) = tokio::join!(call, collector);
            match result {
                Ok(()) => {
                    journal.inflight_provider_id = None;
                    journal.retry_not_before_unix_ms = None;
                    journal.selected_provider_id = Some(route.candidate.id.clone());
                    journal.staged_events = events;
                    Self::persist_model_route_journal(&path, &journal)?;
                    self.provider_health
                        .lock()
                        .map_err(|_| {
                            LocalRuntimeError::StateRoot("Provider health state is poisoned".into())
                        })?
                        .observe_success(&self.model_route_binding_digest, &route.candidate.id)?;
                    self.flush_model_route_observations(
                        run_id,
                        attempt_id,
                        &path,
                        &mut journal,
                        event_types,
                    )?;
                    return Ok((path, journal.staged_events.clone()));
                }
                Err(error) => {
                    journal.inflight_provider_id = None;
                    let failure = Self::route_failure(&route.candidate.id, &error);
                    let circuit_opened = self
                        .provider_health
                        .lock()
                        .map_err(|_| {
                            LocalRuntimeError::StateRoot("Provider health state is poisoned".into())
                        })?
                        .observe_failure(
                            &self.model_route_binding_digest,
                            &route.candidate.id,
                            &journal.invocation_digest,
                            &failure,
                            &self.config.model_routing.health_policy,
                        )?;
                    let committed_events = events
                        .iter()
                        .filter(|event| event.commits_provider_output())
                        .count();
                    if self.can_fallback(&failure, committed_events)
                        && !circuit_opened
                        && journal.same_provider_attempts
                            < self
                                .config
                                .model_routing
                                .health_policy
                                .max_same_provider_attempts
                    {
                        let delay_ms = self
                            .same_provider_retry_delay_ms(&failure, journal.same_provider_attempts);
                        let retry_not_before_unix_ms = Utc::now()
                            .timestamp_millis()
                            .saturating_add(i64::try_from(delay_ms).unwrap_or(i64::MAX));
                        journal.retry_attempts.push(LocalModelRouteRetry {
                            provider_id: route.candidate.id.clone(),
                            provider_attempt: journal.same_provider_attempts.saturating_add(1),
                            failure,
                            delay_ms,
                            retry_not_before_unix_ms,
                        });
                        journal.retry_not_before_unix_ms = Some(retry_not_before_unix_ms);
                        self.flush_model_route_observations(
                            run_id,
                            attempt_id,
                            &path,
                            &mut journal,
                            event_types,
                        )?;
                        continue;
                    }
                    if self.can_fallback(&failure, committed_events)
                        && journal.next_candidate_index + 1 < routes.len()
                    {
                        journal.failed_attempts.push(failure);
                        journal.next_candidate_index += 1;
                        journal.same_provider_attempts = 0;
                        journal.retry_not_before_unix_ms = None;
                        self.flush_model_route_observations(
                            run_id,
                            attempt_id,
                            &path,
                            &mut journal,
                            event_types,
                        )?;
                        continue;
                    }
                    if self.can_fallback(&failure, committed_events) {
                        journal.terminal_failure = Some(failure.clone());
                        self.flush_model_route_observations(
                            run_id,
                            attempt_id,
                            &path,
                            &mut journal,
                            event_types,
                        )?;
                        if journal.same_provider_attempts
                            >= self
                                .config
                                .model_routing
                                .health_policy
                                .max_same_provider_attempts
                        {
                            journal.staged_events = vec![ModelStreamEvent::Failed {
                                kind: failure.kind,
                                retryable: false,
                                message: format!(
                                    "Provider {} exhausted the frozen same-provider attempt budget; diagnostic digest {}",
                                    failure.provider_id, failure.message_digest
                                ),
                            }];
                            Self::persist_model_route_journal(&path, &journal)?;
                            return Ok((path, journal.staged_events.clone()));
                        }
                        return Err(LocalRuntimeError::Provider(format!(
                            "retryable Provider {} failure before output; diagnostic digest {}",
                            failure.provider_id, failure.message_digest
                        )));
                    }
                    events.push(ModelStreamEvent::Failed {
                        kind: failure.kind,
                        retryable: failure.retryable,
                        message: format!(
                            "Provider {} failed; diagnostic digest {}",
                            failure.provider_id, failure.message_digest
                        ),
                    });
                    journal.terminal_failure = Some(failure);
                    journal.staged_events = events;
                    Self::persist_model_route_journal(&path, &journal)?;
                    self.flush_model_route_observations(
                        run_id,
                        attempt_id,
                        &path,
                        &mut journal,
                        event_types,
                    )?;
                    return Ok((path, journal.staged_events.clone()));
                }
            }
        }
    }

    fn complete_model_route_journal(&self, path: &Path) -> Result<(), LocalRuntimeError> {
        let mut journal = Self::read_model_route_journal(path)?;
        journal.staged_events.clear();
        journal.completed = true;
        Self::persist_model_route_journal(path, &journal)
    }

    /// Seals the one model-route WAL that can remain between a terminal
    /// Checkpoint/Event commit and the ordinary post-event completion append.
    ///
    /// Model invocations are serial within one attempt, so more than one
    /// unfinished journal for the terminal attempt is contradictory evidence.
    /// A journal still carrying an inflight call or retry deadline is left
    /// untouched: the terminal Checkpoint prevents replay, but the Runtime must
    /// not rewrite an ambiguous Provider boundary as a completed response.
    fn reconcile_terminal_model_route_wal(
        &self,
        run_id: Uuid,
        checkpoint: &agent_protocol::CheckpointSnapshot,
    ) -> Result<(), LocalRuntimeError> {
        let directory = self.run_dir(run_id).join("model-routes");
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
        };
        let mut unfinished = None;
        for entry in entries {
            let entry = entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
            let file_type = entry
                .file_type()
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
            if !file_type.is_file() {
                return Err(LocalRuntimeError::Checkpoint(
                    "model route directory contains an unexpected entry".into(),
                ));
            }
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                LocalRuntimeError::Checkpoint("model route filename is invalid".into())
            })?;
            if let Some(digest) = name.strip_suffix(".json.partial") {
                if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(LocalRuntimeError::Checkpoint(
                        "model route staging filename is invalid".into(),
                    ));
                }
                // durable_file::replace commits only by renaming this sibling
                // to `<digest>.json`. A crash may leave the staging write, but
                // it is never authoritative beside the committed WAL.
                continue;
            }
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                return Err(LocalRuntimeError::Checkpoint(
                    "model route filename is invalid".into(),
                ));
            }
            let journal = Self::read_model_route_journal(&entry.path())?;
            if journal.run_id != run_id
                || journal.model_route_binding_digest != self.model_route_binding_digest
            {
                return Err(LocalRuntimeError::Checkpoint(
                    "terminal Run model route WAL is bound to another invocation".into(),
                ));
            }
            if journal.completed {
                continue;
            }
            if journal.attempt_id != checkpoint.attempt_id {
                return Err(LocalRuntimeError::Checkpoint(
                    "terminal Checkpoint coexists with an unfinished model route from another attempt"
                        .into(),
                ));
            }
            if unfinished.replace((entry.path(), journal)).is_some() {
                return Err(LocalRuntimeError::Checkpoint(
                    "terminal attempt has multiple unfinished model route WALs".into(),
                ));
            }
        }

        let Some((path, journal)) = unfinished else {
            return Ok(());
        };
        let response_is_settled = journal.inflight_provider_id.is_none()
            && journal.retry_not_before_unix_ms.is_none()
            && ((journal.selected_provider_id.is_some() && journal.selection_reported)
                || (journal.terminal_failure.is_some() && journal.terminal_failure_reported));
        if response_is_settled {
            self.complete_model_route_journal(&path)?;
        }
        Ok(())
    }

    fn session_record_path(state_root: &Path, session_id: Uuid) -> PathBuf {
        state_root
            .join("sessions")
            .join(session_id.to_string())
            .join("session.json")
    }

    fn read_session_record(
        state_root: &Path,
        session_id: Uuid,
    ) -> Result<LocalSessionRecord, LocalRuntimeError> {
        let path = Self::session_record_path(state_root, session_id);
        // "There is no such Session" and "the state root is not readable" are
        // different answers and only one of them is worth retrying. Folding the
        // first into `StateRoot` told every caller its storage was down.
        let body = std::fs::read(&path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                LocalRuntimeError::Execution("no such root Session".into())
            }
            _ => LocalRuntimeError::StateRoot(error.to_string()),
        })?;
        let record: LocalSessionRecord = serde_json::from_slice(&body)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        if record.session_id != session_id || !record.is_well_formed() {
            return Err(LocalRuntimeError::Checkpoint(
                "root Session record is malformed or bound to another identity".into(),
            ));
        }
        Ok(record)
    }

    fn persist_session_record(
        state_root: &Path,
        record: &LocalSessionRecord,
    ) -> Result<(), LocalRuntimeError> {
        if !record.is_well_formed() {
            return Err(LocalRuntimeError::Checkpoint(
                "refusing to persist malformed root Session state".into(),
            ));
        }
        let path = Self::session_record_path(state_root, record.session_id);
        let mut record = record.clone();
        record.store_version = LOCAL_SESSION_STORE_VERSION;
        let body = serde_json::to_vec_pretty(&record)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        durable_file::replace(&path, &body)
    }

    fn read_owned_session_record(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
    ) -> Result<LocalSessionRecord, LocalRuntimeError> {
        let record = Self::read_session_record(state_root, session_id)?;
        if record.invocation != invocation {
            return Err(LocalRuntimeError::Execution(
                "root Session is owned by another invocation".into(),
            ));
        }
        Ok(record)
    }

    fn history_prefix(
        history: &[SessionConversationTurn],
        through_turn_ordinal: u64,
    ) -> Result<Vec<SessionConversationTurn>, LocalRuntimeError> {
        if through_turn_ordinal == 0 {
            return Ok(Vec::new());
        }
        let index = history
            .iter()
            .position(|turn| turn.turn_ordinal == through_turn_ordinal)
            .ok_or_else(|| {
                LocalRuntimeError::Execution(
                    "root Session history does not contain the requested completed Turn".into(),
                )
            })?;
        Ok(history[..=index].to_vec())
    }

    pub(crate) fn find_active_session_turn(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
    ) -> Result<Option<(Uuid, Uuid, u64, String)>, LocalRuntimeError> {
        let entries = match std::fs::read_dir(state_root.join("sessions")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
        };
        let mut found = None;
        for entry in entries {
            let entry = entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
            let Some(session_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| Uuid::parse_str(name).ok())
            else {
                continue;
            };
            let record = Self::read_session_record(state_root, session_id)?;
            if record.invocation != invocation {
                continue;
            }
            for branch in record.branches.values() {
                let Some(active) = branch
                    .active_turn
                    .as_ref()
                    .filter(|active| active.run_id == run_id)
                else {
                    continue;
                };
                if found.is_some() {
                    return Err(LocalRuntimeError::Checkpoint(
                        "one root Run is active in multiple Session branches".into(),
                    ));
                }
                found = Some((
                    session_id,
                    branch.branch_id,
                    branch.generation,
                    active.input.clone(),
                ));
            }
        }
        Ok(found)
    }

    pub(crate) fn list_active_session_turns(
        state_root: &Path,
    ) -> Result<Vec<LocalSessionTurnBinding>, LocalRuntimeError> {
        let entries = match std::fs::read_dir(state_root.join("sessions")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
        };
        let mut bindings = Vec::new();
        let mut run_ids = BTreeSet::new();
        for entry in entries {
            let entry = entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
            let Some(session_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| Uuid::parse_str(name).ok())
            else {
                continue;
            };
            let record = Self::read_session_record(state_root, session_id)?;
            for branch in record.branches.values() {
                let Some(active) = &branch.active_turn else {
                    continue;
                };
                if !run_ids.insert(active.run_id) {
                    return Err(LocalRuntimeError::Checkpoint(
                        "one root Run is active in multiple Session branches".into(),
                    ));
                }
                bindings.push(LocalSessionTurnBinding {
                    invocation: record.invocation,
                    session_id,
                    branch_id: branch.branch_id,
                    generation: active.generation,
                    run_id: active.run_id,
                    input: active.input.clone(),
                });
            }
        }
        bindings.sort_by_key(|binding| (binding.session_id, binding.branch_id, binding.run_id));
        Ok(bindings)
    }

    pub(crate) fn clear_active_session_turn(
        state_root: &Path,
        binding: &LocalSessionTurnBinding,
    ) -> Result<LocalSessionHead, LocalRuntimeError> {
        let mut record =
            Self::read_owned_session_record(state_root, binding.invocation, binding.session_id)?;
        let branch = record.branches.get_mut(&binding.branch_id).ok_or_else(|| {
            LocalRuntimeError::Checkpoint("active root Session branch disappeared".into())
        })?;
        let active = branch.active_turn.as_ref().ok_or_else(|| {
            LocalRuntimeError::Checkpoint("active root Session Turn disappeared".into())
        })?;
        if branch.generation != binding.generation
            || active.run_id != binding.run_id
            || active.generation != binding.generation
            || active.input != binding.input
        {
            return Err(LocalRuntimeError::Checkpoint(
                "active root Session Turn changed during recovery".into(),
            ));
        }
        branch.active_turn = None;
        let head = branch.head(binding.session_id);
        Self::persist_session_record(state_root, &record)?;
        Ok(head)
    }

    /// Returns the Run ids whose hot artifacts are still required to resume a
    /// root Session Turn or unfinished child execution. Completed Session and
    /// subagent histories carry their own digest-bound transcript/result and
    /// therefore keep provenance ids, not strong artifact references.
    pub(crate) fn retention_strong_run_references(
        state_root: &Path,
    ) -> Result<BTreeSet<Uuid>, LocalRuntimeError> {
        let mut references = BTreeSet::new();
        match std::fs::read_dir(state_root.join("sessions")) {
            Ok(entries) => {
                for entry in entries {
                    let entry =
                        entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
                    let Some(session_id) = entry
                        .file_name()
                        .to_str()
                        .and_then(|name| Uuid::parse_str(name).ok())
                    else {
                        continue;
                    };
                    let record = Self::read_session_record(state_root, session_id)?;
                    references.extend(record.branches.values().filter_map(|branch| {
                        branch.active_turn.as_ref().map(|active| active.run_id)
                    }));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
        }

        references.extend(Self::managed_subagent_run_references(state_root)?);
        Ok(references)
    }

    /// Returns unfinished child Runs that remain owned by a parent
    /// Checkpoint. A replacement must resume the root of that graph and let
    /// the parent drive its children; independently dispatching both parent
    /// and child would create two owner-epoch contenders for one child.
    pub(crate) fn managed_subagent_run_references(
        state_root: &Path,
    ) -> Result<BTreeSet<Uuid>, LocalRuntimeError> {
        let mut references = BTreeSet::new();
        match std::fs::read_dir(state_root.join("runs")) {
            Ok(entries) => {
                for entry in entries {
                    let entry =
                        entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
                    let Some(run_id) = entry
                        .file_name()
                        .to_str()
                        .and_then(|name| Uuid::parse_str(name).ok())
                    else {
                        continue;
                    };
                    if Self::read_run_record(state_root, run_id)?.is_some_and(|record| {
                        matches!(
                            record.state,
                            LocalRunState::Finished { .. } | LocalRunState::Cancelled { .. }
                        )
                    }) {
                        continue;
                    }
                    let checkpoint_path = Self::checkpoint_path(state_root, run_id);
                    if !checkpoint_path.is_file() {
                        continue;
                    }
                    let checkpoint = Self::load_checkpoint(&checkpoint_path)?;
                    let state: serde_json::Value = serde_json::from_slice(&checkpoint.state)
                        .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
                    let mut insert_request = |request: &serde_json::Value| {
                        if let Some(run_id) = request
                            .get("delegation_id")
                            .and_then(serde_json::Value::as_str)
                            .and_then(|value| Uuid::parse_str(value).ok())
                        {
                            references.insert(run_id);
                        }
                    };
                    if let Some(request) = state.get("pending_subagent")
                        && !request.is_null()
                    {
                        insert_request(request);
                    }
                    if let Some(requests) = state
                        .get("pending_subagents")
                        .and_then(serde_json::Value::as_array)
                    {
                        for request in requests {
                            insert_request(request);
                        }
                    }
                    if let Some(requests) = state
                        .get("active_subagents")
                        .and_then(serde_json::Value::as_object)
                    {
                        for request in requests.values() {
                            insert_request(request);
                        }
                    }
                    if let Some(reservations) = state
                        .get("subagent_budget_reservations")
                        .and_then(serde_json::Value::as_object)
                    {
                        for run_id in reservations
                            .keys()
                            .filter_map(|value| Uuid::parse_str(value).ok())
                        {
                            references.insert(run_id);
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
        }
        Ok(references)
    }

    fn subagent_result_path(
        state_root: &Path,
        parent_run_id: Uuid,
        delegation_id: Uuid,
    ) -> PathBuf {
        state_root
            .join("runs")
            .join(parent_run_id.to_string())
            .join("subagents")
            .join(format!("{delegation_id}.result.json"))
    }

    fn load_subagent_result(
        state_root: &Path,
        parent_run_id: Uuid,
        request: &agent_protocol::SubagentSpawnRequest,
    ) -> Result<Option<SubagentResultDelivery>, LocalRuntimeError> {
        let path = Self::subagent_result_path(state_root, parent_run_id, request.delegation_id);
        let body = match std::fs::read(path) {
            Ok(body) => body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
        };
        let result: SubagentResultDelivery = serde_json::from_slice(&body)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        if !result.is_well_formed()
            || result.tool_call_id != request.tool_call_id
            || result.delegation_id != request.delegation_id
            || result.binding_digest != request.binding_digest
        {
            return Err(LocalRuntimeError::Checkpoint(
                "persisted subagent result does not match the pending delegation".into(),
            ));
        }
        Ok(Some(result))
    }

    fn persist_subagent_result(
        state_root: &Path,
        parent_run_id: Uuid,
        result: &SubagentResultDelivery,
    ) -> Result<(), LocalRuntimeError> {
        let path = Self::subagent_result_path(state_root, parent_run_id, result.delegation_id);
        let body = serde_json::to_vec_pretty(result)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        durable_file::replace(&path, &body)
    }

    fn completed_subagent_result(
        state_root: &Path,
        request: &agent_protocol::SubagentSpawnRequest,
    ) -> Result<Option<SubagentResultDelivery>, LocalRuntimeError> {
        Self::completed_subagent_result_with_transcript(state_root, request, Vec::new())
    }

    fn completed_subagent_result_with_transcript(
        state_root: &Path,
        request: &agent_protocol::SubagentSpawnRequest,
        mut transcript: Vec<agent_protocol::Message>,
    ) -> Result<Option<SubagentResultDelivery>, LocalRuntimeError> {
        let child_run_id = request.delegation_id;
        let events = Self::replay_events(state_root, child_run_id, 0)?;
        let Some(terminal) = events.iter().rev().find_map(|event| {
            let status = match event.event_type.as_str() {
                "run.succeeded" => RunStatus::Succeeded,
                "run.failed" => RunStatus::Failed,
                "run.cancelled" => RunStatus::Cancelled,
                "run.timed_out" => RunStatus::TimedOut,
                "run.indeterminate" => RunStatus::Indeterminate,
                _ => return None,
            };
            (!event.event_id.is_nil()).then_some((event.event_id, status))
        }) else {
            return Ok(None);
        };
        let output = events
            .iter()
            .filter(|event| event.event_type == "model.output.delta")
            .filter_map(|event| {
                event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
            })
            .collect::<String>();
        let usage = events
            .iter()
            .filter(|event| event.event_type == "model.usage")
            .fold(SubagentBudgetUsage::default(), |mut total, event| {
                total.tokens = total.tokens.saturating_add(
                    event
                        .payload
                        .get("input_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0)
                        .saturating_add(
                            event
                                .payload
                                .get("output_tokens")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0),
                        ),
                );
                total.cost_micros = total.cost_micros.saturating_add(
                    event
                        .payload
                        .get("cost_micros")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                );
                total
            });
        if transcript.is_empty() {
            let checkpoint_path = Self::checkpoint_path(state_root, child_run_id);
            if checkpoint_path.is_file() {
                let checkpoint = Self::load_checkpoint(&checkpoint_path)?;
                if checkpoint.status.is_terminal() {
                    if checkpoint.run_id != child_run_id || checkpoint.status != terminal.1 {
                        return Err(LocalRuntimeError::Checkpoint(
                            "child terminal checkpoint does not match its event log".into(),
                        ));
                    }
                    transcript =
                        WorkerProcessor::conversation_transcript_from_checkpoint(&checkpoint)
                            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
                }
            }
        }
        let source = SubagentResultSource {
            tool_call_id: request.tool_call_id.clone(),
            delegation_id: request.delegation_id,
            binding_digest: request.binding_digest.clone(),
            child_run_id,
            child_terminal_event_id: terminal.0,
        };
        let outcome = SubagentResultOutcome {
            terminal_status: terminal.1,
            content: serde_json::json!({
                "text": output,
                "subagent_run_id": child_run_id,
                "role": request.role,
                "terminal_status": terminal.1,
                "is_error": terminal.1 != RunStatus::Succeeded,
            }),
            is_error: terminal.1 != RunStatus::Succeeded,
        };
        let result = if transcript.is_empty() {
            SubagentResultDelivery::new_with_usage(source, outcome, usage)
        } else {
            SubagentResultDelivery::new_with_usage_and_transcript(
                source, outcome, usage, transcript,
            )
        };
        if !result.is_well_formed() {
            return Err(LocalRuntimeError::Checkpoint(
                "completed subagent transcript is malformed".into(),
            ));
        }
        Ok(Some(result))
    }

    fn persist_checkpoint(
        &self,
        run_id: Uuid,
        attempt_id: Uuid,
    ) -> Result<PathBuf, LocalRuntimeError> {
        let snapshot = self
            .processor
            .checkpoint(attempt_id)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        let path = Self::checkpoint_path(&self.config.state_root, run_id);
        let body = serde_json::json!({
            "store_version": LOCAL_STORE_VERSION,
            "run_id": run_id,
            "checkpoint": snapshot,
        });
        let encoded = serde_json::to_vec_pretty(&body)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        durable_file::replace(&path, &encoded)?;
        Ok(path)
    }

    async fn compact_transcript_if_needed(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        emitted: &mut Vec<String>,
    ) -> Result<bool, LocalRuntimeError> {
        let Some(prepared) = self
            .processor
            .prepare_transcript_compaction(attempt_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
        else {
            return Ok(false);
        };

        // The cut point and its source/tail digests must survive a provider
        // failure or process restart. Recovery then rebuilds exactly this
        // summarization request instead of selecting a different boundary.
        self.persist_checkpoint(run_id, attempt_id)?;
        let request = decode_model_invocation(&prepared.invocation)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let routed = self
            .execute_model_with_frozen_routing(run_id, attempt_id, &request, emitted)
            .await;
        if self.cancellation.is_cancelled() {
            self.terminate_interrupted(run_id, attempt_id, emitted)?;
            return Ok(true);
        }
        let (route_journal_path, events) = routed?;

        let mut summary = String::new();
        let mut input_tokens = 0_u64;
        let mut output_tokens = 0_u64;
        let mut cost_micros = 0_u64;
        let mut completed = false;
        for event in events {
            match event {
                ModelStreamEvent::TextDelta { text } => summary.push_str(&text),
                ModelStreamEvent::Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cost_micros: cost,
                } => {
                    input_tokens = input_tokens.saturating_add(input);
                    output_tokens = output_tokens.saturating_add(output);
                    cost_micros = cost_micros.saturating_add(cost);
                }
                ModelStreamEvent::Completed {
                    reason: agent_protocol::ModelFinishReason::Stop,
                } => completed = true,
                ModelStreamEvent::Completed { reason } => {
                    return Err(LocalRuntimeError::Provider(format!(
                        "transcript compaction ended with {reason:?}"
                    )));
                }
                ModelStreamEvent::ToolCall { .. } => {
                    return Err(LocalRuntimeError::Provider(
                        "transcript compaction attempted a Tool call".into(),
                    ));
                }
                ModelStreamEvent::Reasoning { .. }
                | ModelStreamEvent::PrivateStateOmitted { .. } => {}
                ModelStreamEvent::Refusal { text } => {
                    return Err(LocalRuntimeError::Provider(format!(
                        "transcript compaction was refused: {text}"
                    )));
                }
                ModelStreamEvent::Failed { message, .. } => {
                    return Err(LocalRuntimeError::Provider(format!(
                        "transcript compaction failed: {message}"
                    )));
                }
            }
        }
        if !completed {
            return Err(LocalRuntimeError::Provider(
                "transcript compaction ended without a stop completion".into(),
            ));
        }
        let compacted = self
            .processor
            .apply_transcript_compaction(
                attempt_id,
                &prepared.binding_digest,
                &summary,
                input_tokens,
                output_tokens,
                cost_micros,
            )
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &compacted, emitted)?;
        if !self
            .processor
            .attempt_is_terminal(attempt_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
        {
            self.persist_checkpoint(run_id, attempt_id)?;
        }
        // The summary response remains staged until the compaction mutation
        // and its Checkpoint are durable. A replacement Host can therefore
        // apply the same summary without a second Provider call.
        self.complete_model_route_journal(&route_journal_path)?;
        Ok(true)
    }

    pub fn load_checkpoint(
        path: &Path,
    ) -> Result<agent_protocol::CheckpointSnapshot, LocalRuntimeError> {
        let body = std::fs::read(path)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        if value
            .get("store_version")
            .and_then(serde_json::Value::as_u64)
            != Some(u64::from(LOCAL_STORE_VERSION))
        {
            return Err(LocalRuntimeError::Checkpoint(
                "unsupported local store version".into(),
            ));
        }
        serde_json::from_value(value["checkpoint"].clone())
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))
    }

    fn load_validated_checkpoint_state(
        state_root: &Path,
        run_id: Uuid,
    ) -> Result<(agent_protocol::CheckpointSnapshot, serde_json::Value), LocalRuntimeError> {
        let checkpoint = Self::load_checkpoint(&Self::checkpoint_path(state_root, run_id))?;
        if checkpoint.run_id != run_id || !checkpoint.verify_digest() {
            return Err(LocalRuntimeError::Checkpoint(
                "Checkpoint identity or digest is invalid".into(),
            ));
        }
        let state = serde_json::from_slice(&checkpoint.state)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        Ok((checkpoint, state))
    }

    /// Confirms a decision against the Checkpoint that owns the pending Tool,
    /// not only the adapter's Run projection. The projection is useful for
    /// discovery, but the Worker Checkpoint is the authority that will consume
    /// the decision after a replacement attempt is created.
    pub(crate) fn validate_approval_resolution_checkpoint(
        state_root: &Path,
        root_run_id: Uuid,
        resolution: &LocalApprovalResolution,
    ) -> Result<(), LocalRuntimeError> {
        let (root_checkpoint, root_state) =
            Self::load_validated_checkpoint_state(state_root, root_run_id)?;
        if resolution.target_run_id != root_run_id {
            let mut requests = Vec::new();
            if let Some(request) = root_state.get("pending_subagent")
                && !request.is_null()
            {
                requests.push(
                    serde_json::from_value(request.clone())
                        .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?,
                );
            }
            if let Some(pending) = root_state
                .get("pending_subagents")
                .and_then(serde_json::Value::as_array)
            {
                for request in pending {
                    let request: agent_protocol::SubagentSpawnRequest =
                        serde_json::from_value(request.clone())
                            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
                    if !requests
                        .iter()
                        .any(|existing: &agent_protocol::SubagentSpawnRequest| {
                            existing.delegation_id == request.delegation_id
                        })
                    {
                        requests.push(request);
                    }
                }
            }
            if Self::subagent_resolution_owner_in_state_root(
                state_root,
                root_run_id,
                &requests,
                resolution.target_run_id,
            )?
            .is_none()
            {
                return Err(LocalRuntimeError::Checkpoint(
                    "approval target is not owned by the root Run Checkpoint".into(),
                ));
            }
        }

        let (target_checkpoint, target_state) =
            Self::load_validated_checkpoint_state(state_root, resolution.target_run_id)?;
        if target_checkpoint.tenant_id != root_checkpoint.tenant_id {
            return Err(LocalRuntimeError::Checkpoint(
                "approval target Checkpoint belongs to another tenant".into(),
            ));
        }
        let pending_matches = target_state
            .get("pending_approval")
            .filter(|pending| !pending.is_null())
            .map(|pending| {
                serde_json::from_value::<ToolApprovalRequest>(pending.clone())
                    .map(|pending| {
                        target_checkpoint.status == RunStatus::WaitingApproval
                            && resolution.approval_id == Some(pending.approval_id)
                            && resolution.binding_digest.as_deref()
                                == Some(pending.execution.binding_digest.as_str())
                    })
                    .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))
            })
            .transpose()?
            .unwrap_or(false);
        let applied_matches = resolution.approval_id.is_some_and(|approval_id| {
            target_state
                .get("applied_approval_decisions")
                .and_then(serde_json::Value::as_object)
                .and_then(|decisions| decisions.get(&approval_id.to_string()))
                .is_some_and(|applied| {
                    applied
                        .get("binding_digest")
                        .and_then(serde_json::Value::as_str)
                        == resolution.binding_digest.as_deref()
                        && applied.get("decision").and_then(serde_json::Value::as_str)
                            == Some(match resolution.decision {
                                LocalApprovalDecision::AllowOnce => "allow_once",
                                LocalApprovalDecision::Deny => "deny",
                            })
                })
        });
        if !pending_matches && !applied_matches {
            return Err(LocalRuntimeError::Checkpoint(
                "approval decision does not match a pending or applied Checkpoint binding".into(),
            ));
        }
        Ok(())
    }

    /// Applies the same authority rule to a multi-round MCP response. A local
    /// Run record is an index for adapters; only the Worker Checkpoint proves
    /// which opaque request state and response binding may continue a Tool.
    pub(crate) fn validate_mcp_resolution_checkpoint(
        state_root: &Path,
        run_id: Uuid,
        resolution: &LocalMcpInputResolution,
    ) -> Result<(), LocalRuntimeError> {
        let (checkpoint, state) = Self::load_validated_checkpoint_state(state_root, run_id)?;
        let validates_pending = |pending: &McpInputRequired| {
            let now = Utc::now();
            pending.validate().is_ok()
                && McpInputResolutionCommand {
                    schema_version: agent_protocol::MCP_INPUT_RESOLUTION_SCHEMA_VERSION,
                    message_id: Uuid::from_u128(1),
                    tenant_id: checkpoint.tenant_id,
                    run_id,
                    attempt_id: Uuid::from_u128(2),
                    worker_id: Uuid::from_u128(3),
                    worker_incarnation_id: Uuid::from_u128(4),
                    input_id: resolution.input_id,
                    input_version: resolution.input_version,
                    binding_digest: resolution.binding_digest.clone(),
                    responses: resolution.responses.clone(),
                    issued_at: now,
                    expires_at: now + ChronoDuration::minutes(1),
                }
                .validate_for(pending)
                .is_ok()
        };
        let pending_matches = state
            .get("pending_mcp_input")
            .filter(|pending| !pending.is_null())
            .map(|pending| {
                serde_json::from_value::<McpInputRequired>(pending.clone())
                    .map(|pending| {
                        checkpoint.status == RunStatus::Suspended && validates_pending(&pending)
                    })
                    .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))
            })
            .transpose()?
            .unwrap_or(false);
        let resolved_matches = state
            .get("resolved_mcp_input")
            .filter(|resolved| !resolved.is_null())
            .map(|resolved| {
                serde_json::from_value::<CheckpointResolvedMcpInput>(resolved.clone())
                    .map(|resolved| {
                        validates_pending(&resolved.pending)
                            && resolved.continuation.responses == resolution.responses
                    })
                    .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))
            })
            .transpose()?
            .unwrap_or(false);
        if !pending_matches && !resolved_matches {
            return Err(LocalRuntimeError::Checkpoint(
                "MCP input decision does not match a pending or applied Checkpoint binding".into(),
            ));
        }
        Ok(())
    }

    /// Closes transport sessions owned by this Host before the async runtime
    /// itself exits. In particular, stdio MCP cleanup must await process-group
    /// reaping; a normal struct drop cannot perform that asynchronous work.
    pub async fn shutdown(&mut self) {
        self.cancellation.cancel();
        self.stop_subagent_tasks().await;
        self.mcp_client = None;
        if let Some(stdio) = self.stdio_mcp.take() {
            stdio.shutdown().await;
        }
    }

    async fn stop_subagent_tasks(&mut self) {
        let tasks = std::mem::take(&mut self.subagent_tasks);
        for task in tasks.values() {
            task.cancellation.cancel();
        }
        let timeout = Duration::from_millis(self.config.runtime_policy.tool_execution.timeout_ms);
        for (_, mut task) in tasks {
            if tokio::time::timeout(timeout, &mut task.task).await.is_err() {
                task.task.abort();
                let _ = task.task.await;
            }
        }
    }

    #[must_use]
    pub async fn mcp_lifecycle_snapshot(&self) -> Option<LocalMcpLifecycleSnapshot> {
        match &self.stdio_mcp {
            Some(stdio) => Some(stdio.lifecycle_snapshot().await),
            None => None,
        }
    }

    fn event_log_path(&self, run_id: Uuid) -> PathBuf {
        self.run_dir(run_id).join("events.jsonl")
    }

    /// Drops only a final row that never reached the JSONL commit marker.
    ///
    /// Every successful append writes and syncs one buffer ending in `\n`.
    /// A process crash can leave a strict prefix of that buffer at EOF. That
    /// prefix was never committed and must not be joined to the next event.
    /// Complete rows -- including malformed ones -- are never repaired here;
    /// readers continue to fail closed on those.
    fn repair_uncommitted_event_tail(file: &mut std::fs::File) -> Result<(), LocalRuntimeError> {
        use std::io::{Read as _, Seek as _, SeekFrom};

        let length = file
            .metadata()
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?
            .len();
        if length == 0 {
            return Ok(());
        }
        file.seek(SeekFrom::End(-1))
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        let mut final_byte = [0_u8; 1];
        file.read_exact(&mut final_byte)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        if final_byte[0] == b'\n' {
            return Ok(());
        }

        const SCAN_BLOCK_BYTES: usize = 8 * 1024;
        let mut cursor = length;
        let mut committed_length = 0_u64;
        let mut buffer = vec![0_u8; SCAN_BLOCK_BYTES];
        while cursor > 0 {
            let start = cursor.saturating_sub(SCAN_BLOCK_BYTES as u64);
            let width = (cursor - start) as usize;
            file.seek(SeekFrom::Start(start))
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
            file.read_exact(&mut buffer[..width])
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
            if let Some(index) = buffer[..width].iter().rposition(|byte| *byte == b'\n') {
                committed_length = start + index as u64 + 1;
                break;
            }
            cursor = start;
        }
        file.set_len(committed_length)
            .and_then(|()| file.sync_data())
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        Ok(())
    }

    /// Persists the event, then broadcasts it. The order matters: a client that
    /// reconnects replays from the log, so an event that was broadcast but not
    /// yet durable would be visible to a connected client and invisible to a
    /// reconnecting one.
    fn emit(
        &self,
        run_id: Uuid,
        envelope: &EventEnvelope,
        types: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let terminal = matches!(
            envelope.event_type.as_str(),
            "run.succeeded"
                | "run.failed"
                | "run.cancelled"
                | "run.timed_out"
                | "run.indeterminate"
        );
        if terminal {
            // Every terminal Event now has the same publication receipt. Before
            // it becomes observable, the Checkpoint stores the exact envelope
            // and complete transcript. This makes one-shot, Session and child
            // Runs converge through one recovery rule instead of letting the
            // one-shot path replay a staged model response over an already
            // committed terminal Event.
            self.persist_checkpoint(run_id, envelope.attempt_id)?;
        }
        types.push(envelope.event_type.clone());
        self.append_event(run_id, envelope)
    }

    fn local_event_from_envelope(
        &self,
        run_id: Uuid,
        envelope: &EventEnvelope,
    ) -> Result<LocalEvent, LocalRuntimeError> {
        if envelope.run_id != run_id || envelope.tenant_id != self.invocation.tenant_id {
            return Err(LocalRuntimeError::Checkpoint(
                "event envelope is bound to another Runtime invocation".into(),
            ));
        }
        Ok(LocalEvent {
            event_id: envelope.event_id,
            schema_version: envelope.schema_version,
            tenant_id: self.invocation.tenant_id,
            application_id: self.invocation.application_id,
            workload_identity_id: self.invocation.workload_identity_id,
            workspace_id: self.invocation.workspace_id,
            agent_version_id: self.invocation.agent_version_id,
            model_policy_id: self.invocation.model_policy_id,
            session_id: envelope.session_id,
            sequence: envelope.sequence,
            run_id,
            attempt_id: envelope.attempt_id,
            timestamp: envelope.timestamp,
            trace_id: envelope.trace_id,
            event_type: envelope.event_type.clone(),
            payload: envelope.payload.clone(),
            digest: envelope.digest.clone(),
        })
    }

    fn local_event_is_bound_to_invocation(&self, run_id: Uuid, event: &LocalEvent) -> bool {
        let payload_digest = hex::encode(Sha256::digest(
            serde_json::to_vec(&event.payload)
                .expect("JSON event payload serialization is infallible"),
        ));
        event.schema_version == 1
            && !event.event_id.is_nil()
            && !event.trace_id.is_nil()
            && event.run_id == run_id
            && event.tenant_id == self.invocation.tenant_id
            && event.application_id == self.invocation.application_id
            && event.workload_identity_id == self.invocation.workload_identity_id
            && event.workspace_id == self.invocation.workspace_id
            && event.agent_version_id == self.invocation.agent_version_id
            && event.model_policy_id == self.invocation.model_policy_id
            && event.digest == payload_digest
    }

    fn append_event(
        &self,
        run_id: Uuid,
        envelope: &EventEnvelope,
    ) -> Result<(), LocalRuntimeError> {
        let event = self.local_event_from_envelope(run_id, envelope)?;
        let dir = self.run_dir(run_id);
        std::fs::create_dir_all(&dir)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        let mut line = serde_json::to_vec(&event)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        line.push(b'\n');
        if line.len() > LOCAL_EVENT_LOG_LINE_MAX_BYTES {
            return Err(LocalRuntimeError::StateRoot(format!(
                "durable event log line exceeds {LOCAL_EVENT_LOG_LINE_MAX_BYTES} bytes"
            )));
        }
        use std::io::Write as _;
        let event_log_path = self.event_log_path(run_id);
        let is_new_log = !event_log_path.exists();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(event_log_path)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        Self::repair_uncommitted_event_tail(&mut file)?;
        file.write_all(&line)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        file.sync_data()
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        if is_new_log {
            #[cfg(unix)]
            std::fs::File::open(&dir)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        }
        Ok(())
    }

    /// Converges the only valid state between a terminal Checkpoint commit and
    /// its Event publication. The Checkpoint carries the exact Kernel envelope,
    /// so recovery appends that identity once; it never manufactures a new
    /// terminal event and never resumes model or Tool execution.
    fn reconcile_terminal_event_from_checkpoint(
        &self,
        run_id: Uuid,
        checkpoint: &agent_protocol::CheckpointSnapshot,
    ) -> Result<(), LocalRuntimeError> {
        let receipt = WorkerProcessor::terminal_event_from_checkpoint(checkpoint)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        let events = Self::replay_events(&self.config.state_root, run_id, 0)?;
        let event_prefix_is_valid = events.first().is_none_or(|event| event.sequence == 1)
            && events
                .iter()
                .all(|event| self.local_event_is_bound_to_invocation(run_id, event))
            && events
                .windows(2)
                .all(|pair| pair[0].sequence.checked_add(1) == Some(pair[1].sequence));
        let terminal_position = events.iter().position(|event| {
            matches!(
                event.event_type.as_str(),
                "run.succeeded"
                    | "run.failed"
                    | "run.cancelled"
                    | "run.timed_out"
                    | "run.indeterminate"
            )
        });
        if let Some(position) = terminal_position {
            let terminal = &events[position];
            if !event_prefix_is_valid
                || position + 1 != events.len()
                || receipt.as_ref().is_some_and(|receipt| {
                    self.local_event_from_envelope(run_id, receipt)
                        .map_or(true, |expected| expected != *terminal)
                })
            {
                return Err(LocalRuntimeError::Checkpoint(
                    "durable terminal Event disagrees with its Checkpoint receipt".into(),
                ));
            }
            return self.reconcile_terminal_model_route_wal(run_id, checkpoint);
        }

        let receipt = receipt.ok_or_else(|| {
            LocalRuntimeError::Checkpoint(
                "legacy terminal Checkpoint cannot repair a missing terminal Event".into(),
            )
        })?;
        let last_sequence = events.last().map_or(0, |event| event.sequence);
        if !event_prefix_is_valid || last_sequence.checked_add(1) != Some(receipt.sequence) {
            return Err(LocalRuntimeError::Checkpoint(
                "event prefix cannot accept the Checkpoint-bound terminal receipt".into(),
            ));
        }
        self.append_event(run_id, &receipt)?;
        self.reconcile_terminal_model_route_wal(run_id, checkpoint)
    }

    /// Replays a Run's durable event log from `after_sequence` (exclusive).
    pub fn replay_events(
        state_root: &Path,
        run_id: Uuid,
        after_sequence: u64,
    ) -> Result<Vec<LocalEvent>, LocalRuntimeError> {
        let path = state_root
            .join("runs")
            .join(run_id.to_string())
            .join("events.jsonl");
        let body = match std::fs::read(&path) {
            Ok(body) => body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
        };
        let mut events = Vec::new();
        let committed_length = body
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        if committed_length == 0 {
            return Ok(events);
        }
        for line in body[..committed_length - 1].split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() || line.len() + 1 > LOCAL_EVENT_LOG_LINE_MAX_BYTES {
                return Err(LocalRuntimeError::StateRoot(
                    "durable event log contains an empty or oversized committed row".into(),
                ));
            }
            let event: LocalEvent = serde_json::from_slice(line)
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
            if event.sequence > after_sequence {
                events.push(event);
            }
        }
        Ok(events)
    }

    pub async fn execute(&mut self, input: &str) -> Result<LocalRunOutcome, LocalRuntimeError> {
        self.execute_as(Uuid::now_v7(), input).await
    }

    pub async fn start_session(
        &mut self,
        input: &str,
    ) -> Result<LocalSessionRunOutcome, LocalRuntimeError> {
        let session_id = Uuid::now_v7();
        let branch_id = Uuid::now_v7();
        let run_id = Uuid::now_v7();
        match Self::prepare_session_start(
            &self.config.state_root,
            self.invocation,
            session_id,
            branch_id,
            run_id,
            input,
        )? {
            LocalSessionTurnPreparation::Execute(_) => {
                self.drive_prepared_session_turn(session_id, branch_id, 1, run_id, input, 1)
                    .await
            }
            LocalSessionTurnPreparation::Existing(_) => Err(LocalRuntimeError::Checkpoint(
                "new root Session unexpectedly resolved to an existing Turn".into(),
            )),
        }
    }

    fn session_turn_matches_input(turn: &SessionConversationTurn, input: &str) -> bool {
        turn.transcript.first().is_some_and(|message| {
            message.role == ProtocolRole::User
                && message.content
                    == vec![ProtocolContentPart::Text {
                        text: input.to_owned(),
                    }]
        })
    }

    pub(crate) fn session_turn_binding_matches(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
        branch_id: Uuid,
        run_id: Uuid,
        input: &str,
    ) -> Result<bool, LocalRuntimeError> {
        if !Self::session_record_path(state_root, session_id).is_file() {
            return Ok(false);
        }
        let record = Self::read_owned_session_record(state_root, invocation, session_id)?;
        let Some(branch) = record.branches.get(&branch_id) else {
            return Ok(false);
        };
        Ok(branch
            .active_turn
            .as_ref()
            .is_some_and(|active| active.run_id == run_id && active.input == input)
            || branch
                .history
                .iter()
                .chain(
                    branch
                        .archived_generations
                        .values()
                        .flat_map(|history| history.iter()),
                )
                .any(|turn| turn.run_id == run_id && Self::session_turn_matches_input(turn, input)))
    }

    /// Decides, without writing anything, whether a start is a retry.
    ///
    /// Mirrors `prepare_session_start`'s checks exactly and stops where that
    /// one would begin to persist. Both run under the same Session lock, so the
    /// answer cannot change between deciding here and committing there.
    pub(crate) fn decide_session_start(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
        branch_id: Uuid,
        run_id: Uuid,
        input: &str,
    ) -> Result<LocalSessionTurnDecision, LocalRuntimeError> {
        if session_id.is_nil()
            || branch_id.is_nil()
            || run_id.is_nil()
            || input.trim().is_empty()
            || input.len() > 32_000
        {
            return Err(LocalRuntimeError::Execution(
                "root Session start identity and input must be nonblank and bounded".into(),
            ));
        }
        if !Self::session_record_path(state_root, session_id).is_file() {
            return Ok(LocalSessionTurnDecision::New);
        }
        let record = Self::read_owned_session_record(state_root, invocation, session_id)?;
        let branch = record.branches.get(&branch_id).ok_or_else(|| {
            LocalRuntimeError::Execution(
                "root Session id is already bound to another branch".into(),
            )
        })?;
        let exact_active = branch.active_turn.as_ref().is_some_and(|active| {
            active.run_id == run_id && active.generation == 1 && active.input == input
        });
        let exact_completed = branch
            .history
            .iter()
            .any(|turn| turn.run_id == run_id && Self::session_turn_matches_input(turn, input));
        if branch.generation == 1 && (exact_active || exact_completed) {
            return Ok(LocalSessionTurnDecision::Existing);
        }
        Err(LocalRuntimeError::Execution(
            "root Session start identity is already bound to another mutation".into(),
        ))
    }

    /// The same decision for a continuation.
    pub(crate) fn decide_session_continue(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
        run_id: Uuid,
        input: &str,
    ) -> Result<LocalSessionTurnDecision, LocalRuntimeError> {
        if session_id.is_nil()
            || branch_id.is_nil()
            || run_id.is_nil()
            || generation == 0
            || input.trim().is_empty()
            || input.len() > 32_000
        {
            return Err(LocalRuntimeError::Execution(
                "root Session continuation identity and input must be nonblank and bounded".into(),
            ));
        }
        let record = Self::read_owned_session_record(state_root, invocation, session_id)?;
        let branch = record.branches.get(&branch_id).ok_or_else(|| {
            LocalRuntimeError::Execution("root Session branch does not exist".into())
        })?;
        let exact_active = branch
            .active_turn
            .as_ref()
            .is_some_and(|active| active.run_id == run_id && active.input == input);
        let exact_completed = branch
            .history
            .iter()
            .chain(
                branch
                    .archived_generations
                    .values()
                    .flat_map(|history| history.iter()),
            )
            .any(|turn| turn.run_id == run_id && Self::session_turn_matches_input(turn, input));
        if exact_active || exact_completed {
            return Ok(LocalSessionTurnDecision::Existing);
        }
        if branch.generation != generation {
            return Err(LocalRuntimeError::Execution(format!(
                "stale root Session generation {generation}; current generation is {}",
                branch.generation
            )));
        }
        if branch.active_turn.is_some() {
            return Err(LocalRuntimeError::Execution(
                "root Session branch already has an active Turn".into(),
            ));
        }
        Ok(LocalSessionTurnDecision::New)
    }

    /// Undoes a `prepare_session_start` whose Run never came into existence.
    ///
    /// Deletes the Session this call created, and only that: every field is
    /// checked against what the failed attempt wrote, so a record that has
    /// since gained history, another branch or a different Turn is left alone.
    /// Removing a Session that someone else's work is in would be a worse
    /// outcome than the leak this exists to prevent.
    pub(crate) fn rollback_prepared_session_start(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
        branch_id: Uuid,
        run_id: Uuid,
    ) -> Result<(), LocalRuntimeError> {
        let Ok(record) = Self::read_owned_session_record(state_root, invocation, session_id) else {
            return Ok(());
        };
        if record.branches.len() != 1 {
            return Ok(());
        }
        let Some(branch) = record.branches.get(&branch_id) else {
            return Ok(());
        };
        let untouched = branch.generation == 1
            && branch.history.is_empty()
            && branch.archived_generations.is_empty()
            && branch
                .active_turn
                .as_ref()
                .is_some_and(|active| active.run_id == run_id);
        if !untouched {
            return Ok(());
        }
        let path = Self::session_record_path(state_root, session_id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(LocalRuntimeError::StateRoot(error.to_string())),
        }
    }

    /// Undoes a `prepare_session_continue` whose Run never came into existence.
    ///
    /// Clears only the active Turn this call wrote. History is never touched:
    /// the branch existed before and must be left exactly as continuable as it
    /// was.
    pub(crate) fn rollback_prepared_session_continue(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
        branch_id: Uuid,
        run_id: Uuid,
    ) -> Result<(), LocalRuntimeError> {
        let Ok(mut record) = Self::read_owned_session_record(state_root, invocation, session_id)
        else {
            return Ok(());
        };
        let Some(branch) = record.branches.get_mut(&branch_id) else {
            return Ok(());
        };
        if !branch
            .active_turn
            .as_ref()
            .is_some_and(|active| active.run_id == run_id)
        {
            return Ok(());
        }
        branch.active_turn = None;
        Self::persist_session_record(state_root, &record)
    }

    pub(crate) fn prepare_session_start(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
        branch_id: Uuid,
        run_id: Uuid,
        input: &str,
    ) -> Result<LocalSessionTurnPreparation, LocalRuntimeError> {
        if session_id.is_nil()
            || branch_id.is_nil()
            || run_id.is_nil()
            || input.trim().is_empty()
            || input.len() > 32_000
        {
            return Err(LocalRuntimeError::Execution(
                "root Session start identity and input must be nonblank and bounded".into(),
            ));
        }
        let path = Self::session_record_path(state_root, session_id);
        if path.is_file() {
            let record = Self::read_owned_session_record(state_root, invocation, session_id)?;
            let branch = record.branches.get(&branch_id).ok_or_else(|| {
                LocalRuntimeError::Execution(
                    "root Session id is already bound to another branch".into(),
                )
            })?;
            let exact_active = branch.active_turn.as_ref().is_some_and(|active| {
                active.run_id == run_id && active.generation == 1 && active.input == input
            });
            let exact_completed = branch
                .history
                .iter()
                .any(|turn| turn.run_id == run_id && Self::session_turn_matches_input(turn, input));
            if branch.generation == 1 && (exact_active || exact_completed) {
                return Ok(LocalSessionTurnPreparation::Existing(
                    branch.head(session_id),
                ));
            }
            return Err(LocalRuntimeError::Execution(
                "root Session start identity is already bound to another mutation".into(),
            ));
        }
        let branch = LocalSessionBranchRecord {
            branch_id,
            generation: 1,
            history: Vec::new(),
            archived_generations: BTreeMap::new(),
            active_turn: Some(LocalSessionActiveTurn {
                run_id,
                generation: 1,
                history_digest: agent_protocol::session_conversation_history_digest(&[]),
                input: input.to_owned(),
            }),
        };
        let head = branch.head(session_id);
        Self::persist_session_record(
            state_root,
            &LocalSessionRecord {
                store_version: LOCAL_SESSION_STORE_VERSION,
                invocation,
                session_id,
                branches: BTreeMap::from([(branch_id, branch)]),
            },
        )?;
        Ok(LocalSessionTurnPreparation::Execute(head))
    }

    pub async fn continue_session(
        &mut self,
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
        input: &str,
    ) -> Result<LocalSessionRunOutcome, LocalRuntimeError> {
        let run_id = Uuid::now_v7();
        match Self::prepare_session_continue(
            &self.config.state_root,
            self.invocation,
            session_id,
            branch_id,
            generation,
            run_id,
            input,
        )? {
            LocalSessionTurnPreparation::Execute(_) => {
                self.drive_prepared_session_turn(
                    session_id, branch_id, generation, run_id, input, 1,
                )
                .await
            }
            LocalSessionTurnPreparation::Existing(_) => Err(LocalRuntimeError::Checkpoint(
                "new root Session continuation unexpectedly resolved to an existing Turn".into(),
            )),
        }
    }

    pub(crate) fn prepare_session_continue(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
        run_id: Uuid,
        input: &str,
    ) -> Result<LocalSessionTurnPreparation, LocalRuntimeError> {
        if session_id.is_nil()
            || branch_id.is_nil()
            || run_id.is_nil()
            || generation == 0
            || input.trim().is_empty()
            || input.len() > 32_000
        {
            return Err(LocalRuntimeError::Execution(
                "root Session continuation identity and input must be nonblank and bounded".into(),
            ));
        }
        let mut record = Self::read_owned_session_record(state_root, invocation, session_id)?;
        let branch = record.branches.get_mut(&branch_id).ok_or_else(|| {
            LocalRuntimeError::Execution("root Session branch does not exist".into())
        })?;
        let exact_active = branch
            .active_turn
            .as_ref()
            .is_some_and(|active| active.run_id == run_id && active.input == input);
        let exact_completed = branch
            .history
            .iter()
            .chain(
                branch
                    .archived_generations
                    .values()
                    .flat_map(|history| history.iter()),
            )
            .any(|turn| turn.run_id == run_id && Self::session_turn_matches_input(turn, input));
        if exact_active || exact_completed {
            return Ok(LocalSessionTurnPreparation::Existing(
                branch.head(session_id),
            ));
        }
        if branch.generation != generation {
            return Err(LocalRuntimeError::Execution(format!(
                "stale root Session generation {generation}; current generation is {}",
                branch.generation
            )));
        }
        if branch.active_turn.is_some() {
            return Err(LocalRuntimeError::Execution(
                "root Session branch already has an active Turn".into(),
            ));
        }
        let snapshot = branch.snapshot();
        branch.active_turn = Some(LocalSessionActiveTurn {
            run_id,
            generation,
            history_digest: snapshot.history_digest,
            input: input.to_owned(),
        });
        let head = branch.head(session_id);
        Self::persist_session_record(state_root, &record)?;
        Ok(LocalSessionTurnPreparation::Execute(head))
    }

    pub(crate) async fn drive_prepared_session_turn(
        &mut self,
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
    ) -> Result<LocalSessionRunOutcome, LocalRuntimeError> {
        self.drive_session_turn(
            session_id,
            branch_id,
            generation,
            run_id,
            input,
            owner_epoch,
            None,
            None,
        )
        .await
    }

    pub fn fork_session(
        &self,
        session_id: Uuid,
        source_branch_id: Uuid,
        source_generation: u64,
        through_turn_ordinal: u64,
    ) -> Result<LocalSessionHead, LocalRuntimeError> {
        Self::fork_session_as(
            &self.config.state_root,
            self.invocation,
            session_id,
            source_branch_id,
            source_generation,
            through_turn_ordinal,
            Uuid::now_v7(),
        )
    }

    pub(crate) fn fork_session_as(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
        source_branch_id: Uuid,
        source_generation: u64,
        through_turn_ordinal: u64,
        target_branch_id: Uuid,
    ) -> Result<LocalSessionHead, LocalRuntimeError> {
        if session_id.is_nil()
            || source_branch_id.is_nil()
            || target_branch_id.is_nil()
            || source_generation == 0
            || source_branch_id == target_branch_id
        {
            return Err(LocalRuntimeError::Execution(
                "root Session Fork identity is invalid".into(),
            ));
        }
        let mut record = Self::read_owned_session_record(state_root, invocation, session_id)?;
        let source = record.branches.get(&source_branch_id).ok_or_else(|| {
            LocalRuntimeError::Execution("root Session source branch does not exist".into())
        })?;
        if source.generation != source_generation {
            return Err(LocalRuntimeError::Execution(format!(
                "stale root Session generation {source_generation}; current generation is {}",
                source.generation
            )));
        }
        if source.active_turn.is_some() {
            return Err(LocalRuntimeError::Execution(
                "cannot Fork a root Session branch with an active Turn".into(),
            ));
        }
        let history = Self::history_prefix(&source.history, through_turn_ordinal)?;
        if let Some(existing) = record.branches.get(&target_branch_id) {
            if existing.generation == 1
                && existing.history == history
                && existing.active_turn.is_none()
            {
                return Ok(existing.head(session_id));
            }
            return Err(LocalRuntimeError::Execution(
                "root Session Fork target is already bound to another mutation".into(),
            ));
        }
        let branch = LocalSessionBranchRecord {
            branch_id: target_branch_id,
            generation: 1,
            history,
            archived_generations: BTreeMap::new(),
            active_turn: None,
        };
        let head = branch.head(session_id);
        record.branches.insert(target_branch_id, branch);
        Self::persist_session_record(state_root, &record)?;
        Ok(head)
    }

    pub fn rollback_session(
        &self,
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
        through_turn_ordinal: u64,
    ) -> Result<LocalSessionHead, LocalRuntimeError> {
        Self::rollback_session_at(
            &self.config.state_root,
            self.invocation,
            session_id,
            branch_id,
            generation,
            through_turn_ordinal,
        )
    }

    pub(crate) fn rollback_session_at(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
        through_turn_ordinal: u64,
    ) -> Result<LocalSessionHead, LocalRuntimeError> {
        if session_id.is_nil() || branch_id.is_nil() || generation == 0 {
            return Err(LocalRuntimeError::Execution(
                "root Session Rollback identity is invalid".into(),
            ));
        }
        let mut record = Self::read_owned_session_record(state_root, invocation, session_id)?;
        let branch = record.branches.get_mut(&branch_id).ok_or_else(|| {
            LocalRuntimeError::Execution("root Session branch does not exist".into())
        })?;
        // The lost-response case, and only that. `starts_with` was too weak:
        // after this rollback a Turn may have been appended, and a history of
        // [prefix, later] still begins with [prefix] -- so replaying the old
        // request would be answered as a retry while quietly discarding the
        // later Turn. The branch has to look exactly as this rollback left it:
        // one generation on, history equal to the prefix, nothing active.
        // Overflow is refused rather than saturated, or a branch at u64::MAX
        // would compare equal to its own successor and match falsely.
        if generation.checked_add(1) == Some(branch.generation)
            && branch.active_turn.is_none()
            && branch
                .archived_generations
                .get(&generation)
                .is_some_and(|archived| {
                    Self::history_prefix(archived, through_turn_ordinal)
                        .is_ok_and(|prefix| branch.history == prefix)
                })
        {
            return Ok(branch.head(session_id));
        }
        if branch.generation != generation {
            return Err(LocalRuntimeError::Execution(format!(
                "stale root Session generation {generation}; current generation is {}",
                branch.generation
            )));
        }
        if branch.active_turn.is_some() {
            return Err(LocalRuntimeError::Execution(
                "cannot Rollback a root Session branch with an active Turn".into(),
            ));
        }
        let history = Self::history_prefix(&branch.history, through_turn_ordinal)?;
        if history.len() >= branch.history.len() {
            return Err(LocalRuntimeError::Execution(
                "root Session Rollback must move to an earlier completed Turn".into(),
            ));
        }
        let next_generation = generation.checked_add(1).ok_or_else(|| {
            LocalRuntimeError::Execution("root Session generation overflow".into())
        })?;
        if branch
            .archived_generations
            .insert(generation, branch.history.clone())
            .is_some()
        {
            return Err(LocalRuntimeError::Checkpoint(
                "root Session generation was already archived".into(),
            ));
        }
        branch.generation = next_generation;
        branch.history = history;
        let head = branch.head(session_id);
        Self::persist_session_record(state_root, &record)?;
        Ok(head)
    }

    pub fn session_history(
        &self,
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
    ) -> Result<Vec<SessionConversationTurn>, LocalRuntimeError> {
        Self::session_history_at(
            &self.config.state_root,
            self.invocation,
            session_id,
            branch_id,
            generation,
        )
    }

    pub(crate) fn session_history_at(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
    ) -> Result<Vec<SessionConversationTurn>, LocalRuntimeError> {
        let record = Self::read_owned_session_record(state_root, invocation, session_id)?;
        let branch = record.branches.get(&branch_id).ok_or_else(|| {
            LocalRuntimeError::Execution("root Session branch does not exist".into())
        })?;
        if generation == branch.generation {
            return Ok(branch.history.clone());
        }
        branch
            .archived_generations
            .get(&generation)
            .cloned()
            .ok_or_else(|| {
                LocalRuntimeError::Execution(
                    "root Session generation does not exist or is not archived".into(),
                )
            })
    }

    pub fn session_head(
        &self,
        session_id: Uuid,
        branch_id: Uuid,
    ) -> Result<LocalSessionHead, LocalRuntimeError> {
        Self::session_head_at(
            &self.config.state_root,
            self.invocation,
            session_id,
            branch_id,
        )
    }

    /// Completes a Session head that its own terminal Run has already outrun.
    ///
    /// A Turn's terminal Kernel event is published before the Turn is committed
    /// onto the branch head, so in between, a caller watching events sees a
    /// finished Run on a branch that still holds an active Turn -- and a
    /// `continue` issued on that observation is refused for a conflict the
    /// caller did nothing to cause. Restart recovery already closes exactly
    /// this window after a crash, using the Checkpoint as the authority. This
    /// closes it while the process is still alive, the same way and from the
    /// same authority: no model request, no Tool replay, no second terminal
    /// event.
    ///
    /// Idempotent, and deliberately quiet. Every reason a Turn cannot be
    /// projected yet -- no Checkpoint, an unverifiable one, a branch that moved
    /// underneath -- returns without touching anything, because this runs on
    /// the read path and a read must not start failing over work that recovery
    /// will finish. The caller then reports whatever the head actually says.
    ///
    /// Callers MUST serialise on the Session: this is a read-modify-write and
    /// two unsynchronised projections would each pass the fence and each append
    /// the same Turn.
    pub(crate) fn project_terminal_session_turn(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
        branch_id: Uuid,
    ) -> Result<(), LocalRuntimeError> {
        // A Session that does not exist yet, or that this invocation may not
        // read, has nothing to project. Both are ordinary on the accept path --
        // `start` runs this before the Session is created -- so neither may
        // become a failure to start.
        let Ok(record) = Self::read_owned_session_record(state_root, invocation, session_id) else {
            return Ok(());
        };
        let Some(branch) = record.branches.get(&branch_id) else {
            return Ok(());
        };
        let Some(active) = branch.active_turn.as_ref() else {
            return Ok(());
        };
        // The Checkpoint is the authority, not the Run record. A terminal Turn
        // lands in three stages -- Kernel event, Run record projection, Session
        // head commit -- and a caller can read between any two of them. Keying
        // off the Run record would make this projection wait for a projection,
        // which is how the window stayed open in the first place; recovery
        // already keys off the Checkpoint and this must agree with it.
        let checkpoint_path = Self::checkpoint_path(state_root, active.run_id);
        if !checkpoint_path.is_file() {
            return Ok(());
        }
        let Ok(checkpoint) = Self::load_checkpoint(&checkpoint_path) else {
            return Ok(());
        };
        if !checkpoint.verify_digest() || !checkpoint.status.is_terminal() {
            return Ok(());
        }
        let status = checkpoint.status;
        let transcript = if status == RunStatus::Succeeded {
            match WorkerProcessor::conversation_transcript_from_checkpoint(&checkpoint) {
                Ok(transcript) => Some(transcript),
                Err(_) => return Ok(()),
            }
        } else {
            None
        };
        // Fenced on the active Turn's own generation and input, so a branch
        // that moved between the read above and the write below is refused
        // inside the commit rather than overwritten here.
        let generation = active.generation;
        let run_id = active.run_id;
        let input = active.input.clone();
        drop(record);
        let _ = Self::commit_session_turn(
            state_root,
            session_id,
            branch_id,
            generation,
            run_id,
            &input,
            status,
            transcript.as_deref(),
        );
        Ok(())
    }

    pub(crate) fn session_head_at(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        session_id: Uuid,
        branch_id: Uuid,
    ) -> Result<LocalSessionHead, LocalRuntimeError> {
        let record = Self::read_owned_session_record(state_root, invocation, session_id)?;
        record
            .branches
            .get(&branch_id)
            .map(|branch| branch.head(session_id))
            .ok_or_else(|| {
                LocalRuntimeError::Execution("root Session branch does not exist".into())
            })
    }

    pub(crate) fn list_session_heads_at(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
    ) -> Result<Vec<LocalSessionHead>, LocalRuntimeError> {
        let entries = match std::fs::read_dir(state_root.join("sessions")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
        };
        let mut heads = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
            let Some(session_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| Uuid::parse_str(name).ok())
            else {
                continue;
            };
            let record = Self::read_session_record(state_root, session_id)?;
            if record.invocation != invocation {
                continue;
            }
            heads.extend(
                record
                    .branches
                    .values()
                    .map(|branch| branch.head(session_id)),
            );
        }
        heads.sort_by_key(|head| (head.session_id, head.branch_id));
        Ok(heads)
    }

    #[allow(clippy::too_many_arguments)]
    async fn drive_session_turn(
        &mut self,
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
        checkpoint: Option<agent_protocol::CheckpointSnapshot>,
        resolution: Option<LocalResumeResolution>,
    ) -> Result<LocalSessionRunOutcome, LocalRuntimeError> {
        let record = Self::read_session_record(&self.config.state_root, session_id)?;
        let branch = record.branches.get(&branch_id).ok_or_else(|| {
            LocalRuntimeError::Checkpoint("active root Session branch disappeared".into())
        })?;
        let active = branch.active_turn.as_ref().ok_or_else(|| {
            LocalRuntimeError::Checkpoint("root Session has no active Turn to execute".into())
        })?;
        let snapshot = branch.snapshot();
        if branch.generation != generation
            || active.run_id != run_id
            || active.generation != generation
            || active.history_digest != snapshot.history_digest
            || active.input != input
        {
            return Err(LocalRuntimeError::Execution(
                "stale root Session Turn binding was fenced before execution".into(),
            ));
        }
        let command = self.local_command_with_session_context(
            run_id,
            session_id,
            input,
            owner_epoch,
            AgentLineage {
                root_run_id: run_id,
                parent_run_id: None,
                delegation_id: None,
                depth: 0,
                role: "primary".into(),
            },
            Vec::new(),
            None,
            Some(snapshot),
        );
        Self::persist_managed_run_state(
            &self.config.state_root,
            self.invocation,
            run_id,
            input,
            owner_epoch,
            LocalRunState::Running,
        )?;
        let run = self.drive(command, checkpoint, resolution).await?;
        Self::persist_managed_run_state(
            &self.config.state_root,
            self.invocation,
            run_id,
            input,
            owner_epoch,
            Self::managed_run_state(&run),
        )?;
        let head = if run.status.is_terminal() {
            let transcript = (run.status == RunStatus::Succeeded)
                .then(|| {
                    self.processor
                        .conversation_transcript(run.attempt_id)
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))
                })
                .transpose()?;
            let head = self.finish_session_turn(
                session_id,
                branch_id,
                generation,
                run_id,
                input,
                &run,
                transcript.as_deref(),
            )?;
            self.acknowledge_local_terminal(&run)?;
            head
        } else {
            Self::read_session_record(&self.config.state_root, session_id)?
                .branches
                .get(&branch_id)
                .expect("validated Session branch remains present")
                .head(session_id)
        };
        Ok(LocalSessionRunOutcome { run, head })
    }

    fn acknowledge_local_terminal(
        &mut self,
        outcome: &LocalRunOutcome,
    ) -> Result<(), LocalRuntimeError> {
        let terminal_event_id = Self::replay_events(&self.config.state_root, outcome.run_id, 0)?
            .into_iter()
            .rev()
            .find(|event| {
                matches!(
                    event.event_type.as_str(),
                    "run.succeeded"
                        | "run.failed"
                        | "run.cancelled"
                        | "run.timed_out"
                        | "run.indeterminate"
                )
            })
            .map(|event| event.event_id)
            .filter(|event_id| !event_id.is_nil())
            .ok_or_else(|| {
                LocalRuntimeError::Checkpoint(
                    "terminal root Session Run has no durable terminal event identity".into(),
                )
            })?;
        self.processor
            .acknowledge_terminal(outcome.attempt_id, terminal_event_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))
    }

    fn recover_terminal_session_turn(
        &mut self,
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
        run_id: Uuid,
        input: &str,
        checkpoint: &agent_protocol::CheckpointSnapshot,
    ) -> Result<LocalSessionRunOutcome, LocalRuntimeError> {
        if checkpoint.run_id != run_id
            || checkpoint.session_id != session_id
            || !checkpoint.status.is_terminal()
            || !checkpoint.verify_digest()
        {
            return Err(LocalRuntimeError::Checkpoint(
                "terminal root Session Checkpoint identity is invalid".into(),
            ));
        }
        let record = Self::read_session_record(&self.config.state_root, session_id)?;
        let branch = record.branches.get(&branch_id).ok_or_else(|| {
            LocalRuntimeError::Checkpoint("terminal root Session branch disappeared".into())
        })?;
        let active = branch.active_turn.as_ref().ok_or_else(|| {
            LocalRuntimeError::Execution(
                "terminal root Session result is stale because no active Turn remains".into(),
            )
        })?;
        let expected_branch = branch.snapshot();
        if branch.generation != generation
            || active.run_id != run_id
            || active.generation != generation
            || active.history_digest != expected_branch.history_digest
            || active.input != input
        {
            return Err(LocalRuntimeError::Execution(
                "terminal root Session result was fenced by a newer head".into(),
            ));
        }
        let state: serde_json::Value = serde_json::from_slice(&checkpoint.state)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        let checkpoint_branch: SessionBranchSnapshot =
            serde_json::from_value(state["session_branch"].clone())
                .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        let expected_input_digest = hex::encode(Sha256::digest(input.as_bytes()));
        if checkpoint_branch != expected_branch
            || state["input_digest"].as_str() != Some(expected_input_digest.as_str())
        {
            return Err(LocalRuntimeError::Checkpoint(
                "terminal root Session Checkpoint does not match its active head".into(),
            ));
        }
        self.reconcile_terminal_event_from_checkpoint(run_id, checkpoint)?;
        let transcript = WorkerProcessor::conversation_transcript_from_checkpoint(checkpoint)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        let events = Self::replay_events(&self.config.state_root, run_id, 0)?;
        let terminal = events.iter().rev().find_map(|event| {
            let status = match event.event_type.as_str() {
                "run.succeeded" => RunStatus::Succeeded,
                "run.failed" => RunStatus::Failed,
                "run.cancelled" => RunStatus::Cancelled,
                "run.timed_out" => RunStatus::TimedOut,
                "run.indeterminate" => RunStatus::Indeterminate,
                _ => return None,
            };
            Some((status, event.event_id))
        });
        if terminal.map(|(status, _)| status) != Some(checkpoint.status) {
            return Err(LocalRuntimeError::Checkpoint(
                "terminal root Session event log disagrees with its Checkpoint".into(),
            ));
        }
        let output = events
            .iter()
            .filter(|event| event.event_type == "model.output.delta")
            .filter_map(|event| {
                event
                    .payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
            })
            .collect::<String>();
        let run = LocalRunOutcome {
            run_id,
            attempt_id: checkpoint.attempt_id,
            status: checkpoint.status,
            event_types: events
                .iter()
                .map(|event| event.event_type.clone())
                .collect(),
            output,
            checkpoint_path: Self::checkpoint_path(&self.config.state_root, run_id),
            pending_approval: None,
            pending_mcp_input: None,
            mcp_servers: Vec::new(),
            history_repair: None,
        };
        let head = self.finish_session_turn(
            session_id,
            branch_id,
            generation,
            run_id,
            input,
            &run,
            (run.status == RunStatus::Succeeded).then_some(transcript.as_slice()),
        )?;
        if self
            .processor
            .active_attempt_ids()
            .contains(&checkpoint.attempt_id)
        {
            let terminal_event_id = terminal
                .map(|(_, event_id)| event_id)
                .filter(|event_id| !event_id.is_nil())
                .ok_or_else(|| {
                    LocalRuntimeError::Checkpoint(
                        "terminal root Session event has no durable identity".into(),
                    )
                })?;
            self.processor
                .acknowledge_terminal(checkpoint.attempt_id, terminal_event_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        }
        Ok(LocalSessionRunOutcome { run, head })
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_session_turn(
        &self,
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
        run_id: Uuid,
        input: &str,
        outcome: &LocalRunOutcome,
        transcript: Option<&[agent_protocol::Message]>,
    ) -> Result<LocalSessionHead, LocalRuntimeError> {
        Self::commit_session_turn(
            &self.config.state_root,
            session_id,
            branch_id,
            generation,
            run_id,
            input,
            outcome.status,
            transcript,
        )
    }

    /// Commits one finished Turn onto its branch head, under the fence the
    /// active Turn was accepted with.
    ///
    /// Static and taking only a status rather than a whole outcome, because the
    /// projection path reconstructs a terminal Turn from its Checkpoint and has
    /// no `LocalRunOutcome` to hand. Callers must serialise: this is a
    /// read-modify-write of the Session record, and two callers that both pass
    /// the fence would both append the same Turn.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit_session_turn(
        state_root: &Path,
        session_id: Uuid,
        branch_id: Uuid,
        generation: u64,
        run_id: Uuid,
        input: &str,
        status: RunStatus,
        transcript: Option<&[agent_protocol::Message]>,
    ) -> Result<LocalSessionHead, LocalRuntimeError> {
        let mut record = Self::read_session_record(state_root, session_id)?;
        let branch = record.branches.get_mut(&branch_id).ok_or_else(|| {
            LocalRuntimeError::Checkpoint("terminal root Session branch disappeared".into())
        })?;
        let snapshot = branch.snapshot();
        let active = branch.active_turn.as_ref().ok_or_else(|| {
            LocalRuntimeError::Execution(
                "late root Session result was fenced because its active Turn no longer exists"
                    .into(),
            )
        })?;
        if branch.generation != generation
            || active.run_id != run_id
            || active.generation != generation
            || active.history_digest != snapshot.history_digest
            || active.input != input
        {
            return Err(LocalRuntimeError::Execution(
                "late root Session result was fenced by a newer branch head".into(),
            ));
        }
        if status == RunStatus::Succeeded {
            let transcript = transcript.ok_or_else(|| {
                LocalRuntimeError::Checkpoint(
                    "succeeded root Session Turn has no terminal transcript".into(),
                )
            })?;
            let current_start = transcript
                .iter()
                .rposition(|message| {
                    message.role == ProtocolRole::User
                        && message.content
                            == vec![ProtocolContentPart::Text {
                                text: input.to_owned(),
                            }]
                })
                .ok_or_else(|| {
                    LocalRuntimeError::Checkpoint(
                        "terminal root Session transcript has no bound current input".into(),
                    )
                })?;
            let turn = SessionConversationTurn::new(
                u64::try_from(branch.history.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
                run_id,
                transcript[current_start..].to_vec(),
            );
            if !turn.is_well_formed() {
                return Err(LocalRuntimeError::Checkpoint(
                    "terminal root Session Turn transcript is malformed".into(),
                ));
            }
            branch.history.push(turn);
        }
        branch.active_turn = None;
        let head = branch.head(session_id);
        Self::persist_session_record(state_root, &record)?;
        Ok(head)
    }

    /// Executes a new root Run with explicitly imported lower-authority
    /// history. Admission performs deterministic Tool-pair repair and returns
    /// its audit report; historical Tool calls are never scheduled.
    pub async fn execute_with_imported_history(
        &mut self,
        input: &str,
        history_import: HistoryImport,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        let run_id = Uuid::now_v7();
        let command = self.local_command_with_context(
            run_id,
            input,
            1,
            AgentLineage {
                root_run_id: run_id,
                parent_run_id: None,
                delegation_id: None,
                depth: 0,
                role: "primary".into(),
            },
            Vec::new(),
            Some(history_import),
        );
        self.drive(command, None, None).await
    }

    fn tool_reconciliation_path(
        state_root: &Path,
        source_run_id: Uuid,
        reconciliation_id: Uuid,
    ) -> PathBuf {
        state_root
            .join("runs")
            .join(source_run_id.to_string())
            .join("reconciliations")
            .join(format!("{reconciliation_id}.json"))
    }

    fn load_tool_reconciliation(
        state_root: &Path,
        source_run_id: Uuid,
        reconciliation_id: Uuid,
    ) -> Result<Option<LocalToolReconciliationRecord>, LocalRuntimeError> {
        let path = Self::tool_reconciliation_path(state_root, source_run_id, reconciliation_id);
        let Ok(body) = std::fs::read(path) else {
            return Ok(None);
        };
        let record: LocalToolReconciliationRecord = serde_json::from_slice(&body)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        if record.schema_version != 1 || record.reconciliation_id != reconciliation_id {
            return Err(LocalRuntimeError::Checkpoint(
                "persisted Tool reconciliation identity is invalid".into(),
            ));
        }
        Ok(Some(record))
    }

    fn persist_tool_reconciliation(
        state_root: &Path,
        source_run_id: Uuid,
        record: &LocalToolReconciliationRecord,
    ) -> Result<(), LocalRuntimeError> {
        let path =
            Self::tool_reconciliation_path(state_root, source_run_id, record.reconciliation_id);
        let body = serde_json::to_vec_pretty(record)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        durable_file::replace(&path, &body)
    }

    fn validate_tool_reconciliation_source(
        &self,
        command: &ToolReconciliationCommand,
    ) -> Result<(agent_protocol::CheckpointSnapshot, Vec<ProtocolMessage>), LocalRuntimeError> {
        if command.tenant_id != self.invocation.tenant_id {
            return Err(LocalRuntimeError::Execution(
                "Tool reconciliation targets another tenant".into(),
            ));
        }
        let checkpoint = Self::load_checkpoint(&Self::checkpoint_path(
            &self.config.state_root,
            command.source_run_id,
        ))?;
        if checkpoint.run_id != command.source_run_id
            || checkpoint.status != RunStatus::Indeterminate
            || !checkpoint.verify_digest()
        {
            return Err(LocalRuntimeError::Checkpoint(
                "Tool reconciliation source is not a verified indeterminate Run".into(),
            ));
        }
        let events = Self::replay_events(&self.config.state_root, command.source_run_id, 0)?;
        let terminal = events
            .iter()
            .rev()
            .find(|event| {
                matches!(
                    event.event_type.as_str(),
                    "run.succeeded"
                        | "run.failed"
                        | "run.cancelled"
                        | "run.timed_out"
                        | "run.indeterminate"
                )
            })
            .ok_or_else(|| {
                LocalRuntimeError::Checkpoint(
                    "Tool reconciliation source has no terminal event".into(),
                )
            })?;
        if terminal.event_type != "run.indeterminate"
            || terminal.event_id != command.source_terminal_event_id
            || terminal.run_id != command.source_run_id
            || terminal.payload["tool_call_id"].as_str() != Some(&command.tool_call_id)
            || terminal.payload["binding_digest"].as_str() != Some(&command.binding_digest)
            || terminal.payload["replay_safe"].as_bool() != Some(false)
        {
            return Err(LocalRuntimeError::Execution(
                "Tool reconciliation does not match the terminal uncertainty evidence".into(),
            ));
        }
        let state: serde_json::Value = serde_json::from_slice(&checkpoint.state)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        let outstanding = state["outstanding_tool_calls"]
            .get(&command.tool_call_id)
            .ok_or_else(|| {
                LocalRuntimeError::Checkpoint(
                    "indeterminate Checkpoint lost its outstanding Tool request".into(),
                )
            })?;
        let started = state["started_tool_calls"]
            .get(&command.tool_call_id)
            .ok_or_else(|| {
                LocalRuntimeError::Checkpoint(
                    "indeterminate Checkpoint lost its Tool start evidence".into(),
                )
            })?;
        if outstanding["binding_digest"].as_str() != Some(&command.binding_digest)
            || started["event_id"] != terminal.payload["started_event_id"]
        {
            return Err(LocalRuntimeError::Checkpoint(
                "indeterminate Checkpoint Tool evidence is inconsistent".into(),
            ));
        }
        let transcript = WorkerProcessor::conversation_transcript_from_checkpoint(&checkpoint)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        Ok((checkpoint, transcript))
    }

    fn terminal_local_outcome(
        &self,
        run_id: Uuid,
        checkpoint: &agent_protocol::CheckpointSnapshot,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        if checkpoint.run_id != run_id
            || !checkpoint.status.is_terminal()
            || !checkpoint.verify_digest()
        {
            return Err(LocalRuntimeError::Checkpoint(
                "continuation terminal Checkpoint identity is invalid".into(),
            ));
        }
        self.reconcile_terminal_event_from_checkpoint(run_id, checkpoint)?;
        let events = Self::replay_events(&self.config.state_root, run_id, 0)?;
        let status = events
            .iter()
            .rev()
            .find_map(|event| match event.event_type.as_str() {
                "run.succeeded" => Some(RunStatus::Succeeded),
                "run.failed" => Some(RunStatus::Failed),
                "run.cancelled" => Some(RunStatus::Cancelled),
                "run.timed_out" => Some(RunStatus::TimedOut),
                "run.indeterminate" => Some(RunStatus::Indeterminate),
                _ => None,
            });
        if status != Some(checkpoint.status) {
            return Err(LocalRuntimeError::Checkpoint(
                "continuation terminal event disagrees with its Checkpoint".into(),
            ));
        }
        let state: serde_json::Value = serde_json::from_slice(&checkpoint.state)
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        let history_repair = serde_json::from_value(state["history_repair"].clone())
            .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
        Ok(LocalRunOutcome {
            run_id,
            attempt_id: checkpoint.attempt_id,
            status: checkpoint.status,
            event_types: events
                .iter()
                .map(|event| event.event_type.clone())
                .collect(),
            output: events
                .iter()
                .filter(|event| event.event_type == "model.output.delta")
                .filter_map(|event| event.payload["text"].as_str())
                .collect(),
            checkpoint_path: Self::checkpoint_path(&self.config.state_root, run_id),
            pending_approval: None,
            pending_mcp_input: None,
            mcp_servers: Vec::new(),
            history_repair,
        })
    }

    async fn execute_as_with_imported_history(
        &mut self,
        run_id: Uuid,
        input: &str,
        history_import: HistoryImport,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        let command = self.local_command_with_context(
            run_id,
            input,
            1,
            AgentLineage {
                root_run_id: run_id,
                parent_run_id: None,
                delegation_id: None,
                depth: 0,
                role: "primary".into(),
            },
            Vec::new(),
            Some(history_import),
        );
        self.drive(command, None, None).await
    }

    pub async fn reconcile_tool_outcome(
        &mut self,
        command: ToolReconciliationCommand,
    ) -> Result<LocalToolReconciliationOutcome, LocalRuntimeError> {
        command
            .validate()
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let (source_checkpoint, mut transcript) =
            self.validate_tool_reconciliation_source(&command)?;
        let mut record = Self::load_tool_reconciliation(
            &self.config.state_root,
            command.source_run_id,
            command.reconciliation_id,
        )?
        .unwrap_or_else(|| LocalToolReconciliationRecord {
            schema_version: 1,
            reconciliation_id: command.reconciliation_id,
            versions: BTreeMap::new(),
            continuation_outcome: None,
        });
        let latest = record.versions.last_key_value();
        let duplicate = match record.versions.get(&command.version) {
            Some(existing)
                if existing == &command
                    && latest.map(|(version, _)| *version) == Some(command.version) =>
            {
                true
            }
            Some(_) => {
                return Err(LocalRuntimeError::Execution(
                    "Tool reconciliation version conflict".into(),
                ));
            }
            None => false,
        };
        if !duplicate
            && !matches!(command.decision, ToolReconciliationDecision::Unresolved)
            && self.run_dir(command.reconciliation_id).exists()
        {
            return Err(LocalRuntimeError::Execution(
                "Tool reconciliation id already belongs to another Run".into(),
            ));
        }
        if !duplicate {
            match latest {
                None if command.version == 1 => {}
                Some((version, previous))
                    if command.version == version.saturating_add(1)
                        && matches!(previous.decision, ToolReconciliationDecision::Unresolved)
                        && previous.tenant_id == command.tenant_id
                        && previous.source_run_id == command.source_run_id
                        && previous.source_terminal_event_id
                            == command.source_terminal_event_id
                        && previous.tool_call_id == command.tool_call_id
                        && previous.binding_digest == command.binding_digest => {}
                _ => {
                    return Err(LocalRuntimeError::Execution(
                        "Tool reconciliation version conflict".into(),
                    ));
                }
            }
            record.versions.insert(command.version, command.clone());
            record.continuation_outcome = None;
            Self::persist_tool_reconciliation(
                &self.config.state_root,
                command.source_run_id,
                &record,
            )?;
        }
        if matches!(command.decision, ToolReconciliationDecision::Unresolved) {
            return Ok(LocalToolReconciliationOutcome {
                source_run_id: command.source_run_id,
                reconciliation_id: command.reconciliation_id,
                version: command.version,
                decision: command.decision,
                continuation: None,
            });
        }
        if let Some(outcome) = &record.continuation_outcome {
            return Ok(LocalToolReconciliationOutcome {
                source_run_id: command.source_run_id,
                reconciliation_id: command.reconciliation_id,
                version: command.version,
                decision: command.decision,
                continuation: Some(outcome.clone()),
            });
        }
        match self.processor.acknowledge_terminal(
            source_checkpoint.attempt_id,
            command.source_terminal_event_id,
        ) {
            Ok(()) | Err(agent_runtime_worker::WorkerAssignmentError::UnknownAttempt) => {}
            Err(error) => return Err(LocalRuntimeError::Execution(error.to_string())),
        }

        let result = match &command.decision {
            ToolReconciliationDecision::Applied { content, is_error } => serde_json::json!({
                "reconciliation": {
                    "decision": "applied",
                    "version": command.version,
                    "operator_id": command.operator_id,
                    "source_run_id": command.source_run_id,
                    "source_terminal_event_id": command.source_terminal_event_id,
                    "is_error": is_error,
                    "replay_safe": false
                },
                "result": content
            }),
            ToolReconciliationDecision::NotApplied => serde_json::json!({
                "reconciliation": {
                    "decision": "not_applied",
                    "version": command.version,
                    "operator_id": command.operator_id,
                    "source_run_id": command.source_run_id,
                    "source_terminal_event_id": command.source_terminal_event_id,
                    "replay_safe": false
                },
                "error": {"code": "operator_confirmed_not_applied"}
            }),
            ToolReconciliationDecision::Unresolved => unreachable!("handled above"),
        };
        transcript.push(ProtocolMessage {
            role: ProtocolRole::Tool,
            content: vec![ProtocolContentPart::ToolResult {
                tool_call_id: command.tool_call_id.clone(),
                content: result,
            }],
        });
        let history_import = HistoryImport {
            schema_version: 1,
            source: HistoryImportSource::External,
            messages: transcript,
        };
        let continuation_input = command
            .continuation_input
            .as_deref()
            .expect("validated final reconciliation has continuation input");
        let continuation_run_id = command.reconciliation_id;
        let continuation_checkpoint_path =
            Self::checkpoint_path(&self.config.state_root, continuation_run_id);
        let continuation = if continuation_checkpoint_path.is_file() {
            let checkpoint = Self::load_checkpoint(&continuation_checkpoint_path)?;
            if checkpoint.status.is_terminal() {
                self.terminal_local_outcome(continuation_run_id, &checkpoint)?
            } else {
                let state: serde_json::Value = serde_json::from_slice(&checkpoint.state)
                    .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
                let owner_epoch = state["owner_epoch"]
                    .as_u64()
                    .and_then(|epoch| epoch.checked_add(1))
                    .ok_or_else(|| {
                        LocalRuntimeError::Checkpoint(
                            "continuation Checkpoint has no recoverable owner epoch".into(),
                        )
                    })?;
                self.resume_with_imported_history(
                    continuation_run_id,
                    continuation_input,
                    owner_epoch,
                    history_import,
                )
                .await?
            }
        } else {
            self.execute_as_with_imported_history(
                continuation_run_id,
                continuation_input,
                history_import,
            )
            .await?
        };
        record.continuation_outcome = Some(continuation.clone());
        Self::persist_tool_reconciliation(&self.config.state_root, command.source_run_id, &record)?;
        Ok(LocalToolReconciliationOutcome {
            source_run_id: command.source_run_id,
            reconciliation_id: command.reconciliation_id,
            version: command.version,
            decision: command.decision,
            continuation: Some(continuation),
        })
    }

    fn arm_duration_deadline(
        &self,
        attempt_id: Uuid,
    ) -> Result<DurationDeadlineGuard, LocalRuntimeError> {
        let remaining = self
            .processor
            .remaining_duration(attempt_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let stop = CancellationToken::new();
        if remaining.is_zero() {
            self.duration_expired.store(true, Ordering::Release);
            self.cancellation.cancel();
            return Ok(DurationDeadlineGuard { stop });
        }
        let duration_expired = self.duration_expired.clone();
        let cancellation = self.cancellation.clone();
        let stop_for_task = stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(remaining) => {
                    // An earlier operator/ancestor cancellation owns the
                    // terminal reason; a later deadline must not relabel it.
                    if !cancellation.is_cancelled() {
                        duration_expired.store(true, Ordering::Release);
                        cancellation.cancel();
                    }
                }
                () = stop_for_task.cancelled() => {}
            }
        });
        Ok(DurationDeadlineGuard { stop })
    }

    fn terminate_interrupted(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let event = if self.duration_expired.load(Ordering::Acquire) {
            self.processor.timeout_duration(attempt_id)
        } else {
            self.processor.cancel(attempt_id)
        }
        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        // An interruption can race with a remote/non-idempotent side effect.
        // Persist that terminal uncertainty before publishing it so operator
        // reconciliation never observes an event backed only by a stale
        // Running Checkpoint.
        if event.event_type == "run.indeterminate" {
            self.persist_checkpoint(run_id, attempt_id)?;
        }
        self.emit(run_id, &event, emitted)
    }

    /// Converts a Provider-layer failure that happens after `run.started` into
    /// the Run's durable terminal fact. Returning the transport error here
    /// would leave the record terminal but the event log non-terminal, making
    /// every external event cursor correctly reject the Run as corrupt.
    fn terminate_provider_failure(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        message: String,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let event = self
            .processor
            .apply_model_event(
                attempt_id,
                ModelStreamEvent::Failed {
                    kind: ModelErrorKind::Unavailable,
                    retryable: false,
                    message,
                },
            )
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &event, emitted)?;
        self.persist_checkpoint(run_id, attempt_id)?;
        Ok(())
    }

    /// Executes under a caller-supplied Run id so a daemon can hand the id to a
    /// client before the work starts, and so the client can attach to the event
    /// log immediately.
    pub async fn execute_as(
        &mut self,
        run_id: Uuid,
        input: &str,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        self.execute_as_at_epoch(run_id, input, 1).await
    }

    /// Executes a caller-addressed Run under an externally assigned Workspace
    /// owner epoch. Edge and embedded adapters use this to preserve the same
    /// fencing history that authorized the task instead of silently replacing
    /// it with the standalone default.
    pub async fn execute_as_at_epoch(
        &mut self,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        let command = self.local_command(run_id, input, owner_epoch);
        self.drive(command, None, None).await
    }

    /// Resumes a Run from its local Checkpoint on a fresh attempt. Restore
    /// re-derives the effective instructions, Tool catalog, and Skill identity
    /// and refuses the Checkpoint when any of them changed.
    pub async fn resume(
        &mut self,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        if let Some((session_id, branch_id, generation, bound_input)) =
            Self::find_active_session_turn(&self.config.state_root, self.invocation, run_id)?
        {
            if input != bound_input {
                return Err(LocalRuntimeError::Execution(
                    "root Session recovery input does not match the active Turn".into(),
                ));
            }
            let checkpoint =
                Self::load_checkpoint(&Self::checkpoint_path(&self.config.state_root, run_id))?;
            if checkpoint.status.is_terminal() {
                return self
                    .recover_terminal_session_turn(
                        session_id,
                        branch_id,
                        generation,
                        run_id,
                        input,
                        &checkpoint,
                    )
                    .map(|outcome| outcome.run);
            }
            return self
                .drive_session_turn(
                    session_id,
                    branch_id,
                    generation,
                    run_id,
                    input,
                    owner_epoch,
                    Some(checkpoint),
                    None,
                )
                .await
                .map(|outcome| outcome.run);
        }
        let checkpoint =
            Self::load_checkpoint(&Self::checkpoint_path(&self.config.state_root, run_id))?;
        let command = self.local_command(run_id, input, owner_epoch);
        if checkpoint.status.is_terminal() {
            self.processor
                .validate_terminal_checkpoint_binding(&command, &checkpoint)
                .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
            return self.terminal_local_outcome(run_id, &checkpoint);
        }
        self.drive(command, Some(checkpoint), None).await
    }

    /// Restores a Run created through `execute_with_imported_history`. The
    /// caller must supply the same raw import; its source and repaired digests
    /// are checked against the durable Checkpoint before model egress.
    pub async fn resume_with_imported_history(
        &mut self,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
        history_import: HistoryImport,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        let checkpoint =
            Self::load_checkpoint(&Self::checkpoint_path(&self.config.state_root, run_id))?;
        let command = self.local_command_with_context(
            run_id,
            input,
            owner_epoch,
            AgentLineage {
                root_run_id: run_id,
                parent_run_id: None,
                delegation_id: None,
                depth: 0,
                role: "primary".into(),
            },
            Vec::new(),
            Some(history_import),
        );
        if checkpoint.status.is_terminal() {
            self.processor
                .validate_terminal_checkpoint_binding(&command, &checkpoint)
                .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
            return self.terminal_local_outcome(run_id, &checkpoint);
        }
        self.drive(command, Some(checkpoint), None).await
    }

    /// Resumes a parked Run and answers the approval it was waiting on. The
    /// pending approval survives in the Checkpoint, so the decision is applied
    /// to the restored attempt after rebinding it to that attempt.
    pub async fn resume_with_decision(
        &mut self,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
        decision: LocalApprovalDecision,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        self.resume_with_resolution(
            run_id,
            input,
            owner_epoch,
            LocalApprovalResolution {
                target_run_id: run_id,
                approval_id: None,
                binding_digest: None,
                decision,
            },
        )
        .await
    }

    pub(crate) async fn resume_with_resolution(
        &mut self,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
        resolution: LocalApprovalResolution,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        if let Some((session_id, branch_id, generation, bound_input)) =
            Self::find_active_session_turn(&self.config.state_root, self.invocation, run_id)?
        {
            if input != bound_input {
                return Err(LocalRuntimeError::Execution(
                    "root Session recovery input does not match the active Turn".into(),
                ));
            }
            let checkpoint =
                Self::load_checkpoint(&Self::checkpoint_path(&self.config.state_root, run_id))?;
            return self
                .drive_session_turn(
                    session_id,
                    branch_id,
                    generation,
                    run_id,
                    input,
                    owner_epoch,
                    Some(checkpoint),
                    Some(LocalResumeResolution::Approval(resolution)),
                )
                .await
                .map(|outcome| outcome.run);
        }
        let checkpoint =
            Self::load_checkpoint(&Self::checkpoint_path(&self.config.state_root, run_id))?;
        let command = self.local_command(run_id, input, owner_epoch);
        self.drive(
            command,
            Some(checkpoint),
            Some(LocalResumeResolution::Approval(resolution)),
        )
        .await
    }

    /// Resumes a stateless MCP 2026 Tool round from its durable input request.
    /// The Host rebuilds execution identity from the replacement attempt; the
    /// caller supplies only the exact input receipt and bounded responses.
    pub async fn resume_with_mcp_input(
        &mut self,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
        resolution: LocalMcpInputResolution,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        if let Some((session_id, branch_id, generation, bound_input)) =
            Self::find_active_session_turn(&self.config.state_root, self.invocation, run_id)?
        {
            if input != bound_input {
                return Err(LocalRuntimeError::Execution(
                    "root Session recovery input does not match the active Turn".into(),
                ));
            }
            let checkpoint =
                Self::load_checkpoint(&Self::checkpoint_path(&self.config.state_root, run_id))?;
            return self
                .drive_session_turn(
                    session_id,
                    branch_id,
                    generation,
                    run_id,
                    input,
                    owner_epoch,
                    Some(checkpoint),
                    Some(LocalResumeResolution::McpInput(resolution)),
                )
                .await
                .map(|outcome| outcome.run);
        }
        let checkpoint =
            Self::load_checkpoint(&Self::checkpoint_path(&self.config.state_root, run_id))?;
        let command = self.local_command(run_id, input, owner_epoch);
        self.drive(
            command,
            Some(checkpoint),
            Some(LocalResumeResolution::McpInput(resolution)),
        )
        .await
    }

    #[must_use]
    pub fn checkpoint_path(state_root: &Path, run_id: Uuid) -> PathBuf {
        state_root
            .join("runs")
            .join(run_id.to_string())
            .join("checkpoint.json")
    }

    fn record_path(state_root: &Path, run_id: Uuid) -> PathBuf {
        state_root
            .join("runs")
            .join(run_id.to_string())
            .join("run.json")
    }

    pub fn write_run_record(
        state_root: &Path,
        record: &LocalRunRecord,
    ) -> Result<(), LocalRuntimeError> {
        let path = Self::record_path(state_root, record.run_id);
        let body = serde_json::to_vec_pretty(record)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        durable_file::replace(&path, &body)
    }

    fn managed_run_state(outcome: &LocalRunOutcome) -> LocalRunState {
        if let Some(approval) = &outcome.pending_approval {
            return LocalRunState::AwaitingApproval {
                approval_id: approval.approval_id,
                binding_digest: approval.binding_digest.clone(),
                target_run_id: Some(approval.target_run_id),
            };
        }
        if let Some(input) = &outcome.pending_mcp_input {
            return LocalRunState::AwaitingMcpInput {
                input: input.clone(),
            };
        }
        match outcome.status {
            RunStatus::Cancelled => LocalRunState::Cancelled {
                reason: "the Runtime execution was cancelled".into(),
            },
            status => LocalRunState::Finished {
                status: status.as_str().into(),
            },
        }
    }

    fn persist_managed_run_state(
        state_root: &Path,
        invocation: RuntimeInvocationContext,
        run_id: Uuid,
        input: &str,
        owner_epoch: u64,
        state: LocalRunState,
    ) -> Result<(), LocalRuntimeError> {
        let existing = Self::read_run_record(state_root, run_id)?;
        if let Some(existing) = &existing {
            let existing_invocation = RuntimeInvocationContext {
                schema_version: 1,
                tenant_id: existing.tenant_id,
                application_id: existing.application_id,
                workload_identity_id: existing.workload_identity_id,
                workspace_id: existing.workspace_id,
                agent_version_id: existing.agent_version_id,
                model_policy_id: existing.model_policy_id,
            };
            if existing_invocation != invocation
                || existing.input != input
                || existing.owner_epoch > owner_epoch
            {
                return Err(LocalRuntimeError::Checkpoint(
                    "managed Run record conflicts with its durable invocation".into(),
                ));
            }
        }
        Self::write_run_record(
            state_root,
            &LocalRunRecord {
                store_version: LOCAL_STORE_VERSION,
                tenant_id: invocation.tenant_id,
                application_id: invocation.application_id,
                workload_identity_id: invocation.workload_identity_id,
                workspace_id: invocation.workspace_id,
                agent_version_id: invocation.agent_version_id,
                model_policy_id: invocation.model_policy_id,
                run_id,
                input: input.to_owned(),
                state,
                owner_epoch,
            },
        )
    }

    pub fn read_run_record(
        state_root: &Path,
        run_id: Uuid,
    ) -> Result<Option<LocalRunRecord>, LocalRuntimeError> {
        let body = match std::fs::read(Self::record_path(state_root, run_id)) {
            Ok(body) => body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
        };
        let record: LocalRunRecord = serde_json::from_slice(&body)
            .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
        if record.store_version != LOCAL_STORE_VERSION {
            return Err(LocalRuntimeError::StateRoot(
                "unsupported local store version".into(),
            ));
        }
        Ok(Some(record))
    }

    /// Every Run this state root knows about, oldest first by Run id.
    pub fn list_run_records(state_root: &Path) -> Result<Vec<LocalRunRecord>, LocalRuntimeError> {
        let entries = match std::fs::read_dir(state_root.join("runs")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(LocalRuntimeError::StateRoot(error.to_string())),
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
            let Some(run_id) = entry
                .file_name()
                .to_str()
                .and_then(|name| Uuid::parse_str(name).ok())
            else {
                continue;
            };
            if let Some(record) = Self::read_run_record(state_root, run_id)? {
                records.push(record);
            }
        }
        records.sort_by_key(|record| record.run_id);
        Ok(records)
    }

    async fn drive(
        &mut self,
        command: RunExecutionCommand,
        checkpoint: Option<agent_protocol::CheckpointSnapshot>,
        resolution: Option<LocalResumeResolution>,
    ) -> Result<LocalRunOutcome, LocalRuntimeError> {
        let (mut resolution, mut mcp_resolution) = match resolution {
            Some(LocalResumeResolution::Approval(resolution)) => (Some(resolution), None),
            Some(LocalResumeResolution::McpInput(resolution)) => (None, Some(resolution)),
            None => (None, None),
        };
        self.pending_mcp_input = None;
        if let Some(manager) = &self.process_session_manager {
            let report = manager
                .sweep()
                .await
                .map_err(|error| LocalRuntimeError::StateRoot(error.to_string()))?;
            if report.indeterminate > 0 {
                return Err(LocalRuntimeError::StateRoot(format!(
                    "{} persistent process session(s) require manual reconciliation",
                    report.indeterminate
                )));
            }
        }
        let run_id = command.run_id;
        let attempt_id = command.attempt_id;
        let now = Utc::now();
        let mut event_types = Vec::new();
        let mut mcp_statuses = Vec::new();
        self.duration_expired.store(false, Ordering::Release);

        let restored_event = match checkpoint {
            Some(snapshot) => {
                let receipt = self
                    .processor
                    .restore(command.clone(), snapshot, now)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                Some(receipt.event)
            }
            None => {
                self.processor
                    .accept(command.clone(), now)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                None
            }
        };
        self.processor
            .bind_cancellation_token(attempt_id, self.cancellation.child_token())
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let _duration_deadline = self.arm_duration_deadline(attempt_id)?;

        // Cancellation intent is allowed to outlive the daemon that accepted
        // it. A replacement restores the exact attempt only to close it; it
        // must not rediscover MCP servers, invoke a model, or execute a Tool.
        if self.cancellation.is_cancelled() {
            if let Some(restored) = restored_event.as_ref() {
                self.emit(run_id, restored, &mut event_types)?;
            }
            self.terminate_interrupted(run_id, attempt_id, &mut event_types)?;
            return Ok(LocalRunOutcome {
                run_id,
                attempt_id,
                status: self
                    .processor
                    .status(attempt_id)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?,
                event_types,
                output: String::new(),
                checkpoint_path: Self::checkpoint_path(&self.config.state_root, run_id),
                pending_approval: None,
                pending_mcp_input: None,
                mcp_servers: Vec::new(),
                history_repair: self
                    .processor
                    .history_repair_report(attempt_id)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?,
            });
        }

        if command.mcp_servers.is_empty() {
            if let Some(restored) = restored_event.as_ref() {
                self.emit(run_id, restored, &mut event_types)?;
            } else {
                let started = self
                    .processor
                    .start(attempt_id)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                self.emit(run_id, &started, &mut event_types)?;
            }
        } else {
            let client = self.mcp_client.clone().ok_or_else(|| {
                LocalRuntimeError::Configuration(
                    "local MCP servers require an in-process federation backend".into(),
                )
            })?;
            let mut coordinator = McpDiscoveryCoordinator::new(usize::from(
                self.config
                    .runtime_policy
                    .mcp_discovery
                    .max_concurrent_servers,
            ));
            if !coordinator
                .start(&self.processor, client, attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
            {
                return Err(LocalRuntimeError::Execution(
                    "MCP discovery for the local attempt was already active".into(),
                ));
            }
            let timeout =
                Duration::from_millis(self.config.runtime_policy.mcp_discovery.total_timeout_ms)
                    .saturating_add(Duration::from_secs(1));
            let completion = coordinator
                .recv_and_apply(&mut self.processor, timeout)
                .await
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
                .ok_or_else(|| {
                    LocalRuntimeError::Execution(
                        "MCP discovery produced no completion before the local deadline".into(),
                    )
                })?;
            match completion {
                McpDiscoveryCompletion::Started {
                    event, mcp_servers, ..
                } => {
                    mcp_statuses = mcp_servers;
                    if restored_event.is_some() {
                        return Err(LocalRuntimeError::Execution(
                            "restored attempt was incorrectly started as new".into(),
                        ));
                    }
                    self.emit(run_id, &event, &mut event_types)?;
                }
                McpDiscoveryCompletion::Restored { mcp_servers, .. } => {
                    mcp_statuses = mcp_servers;
                    let event = restored_event.as_ref().ok_or_else(|| {
                        LocalRuntimeError::Execution(
                            "new attempt was incorrectly classified as restored".into(),
                        )
                    })?;
                    self.emit(run_id, event, &mut event_types)?;
                }
                McpDiscoveryCompletion::Failed {
                    event, mcp_servers, ..
                } => {
                    self.emit(run_id, &event, &mut event_types)?;
                    let checkpoint_path = self.persist_checkpoint(run_id, attempt_id)?;
                    return Ok(LocalRunOutcome {
                        run_id,
                        attempt_id,
                        status: self
                            .processor
                            .status(attempt_id)
                            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?,
                        event_types,
                        output: String::new(),
                        checkpoint_path,
                        pending_approval: None,
                        pending_mcp_input: None,
                        mcp_servers,
                        history_repair: self
                            .processor
                            .history_repair_report(attempt_id)
                            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?,
                    });
                }
                McpDiscoveryCompletion::Cancelled { .. } => {
                    if let Some(restored) = restored_event.as_ref() {
                        self.emit(run_id, restored, &mut event_types)?;
                    }
                    self.terminate_interrupted(run_id, attempt_id, &mut event_types)?;
                    return Ok(LocalRunOutcome {
                        run_id,
                        attempt_id,
                        status: self
                            .processor
                            .status(attempt_id)
                            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?,
                        event_types,
                        output: String::new(),
                        checkpoint_path: Self::checkpoint_path(&self.config.state_root, run_id),
                        pending_approval: None,
                        pending_mcp_input: None,
                        mcp_servers: Vec::new(),
                        history_repair: self
                            .processor
                            .history_repair_report(attempt_id)
                            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?,
                    });
                }
            }
        }

        let mut output = String::new();
        let mut pending_approval = None;
        let mut checkpoint_path = Self::checkpoint_path(&self.config.state_root, run_id);
        let recovery_action = restored_event
            .is_some()
            .then(|| self.processor.recovery_action(attempt_id))
            .transpose()
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;

        if let Some(WorkerRecoveryAction::WaitForMcpInput(pending)) = recovery_action.as_ref() {
            if let Some(resolution) = mcp_resolution.take() {
                let issued_at = Utc::now();
                let resolved = self
                    .processor
                    .apply_mcp_input_resolution(
                        McpInputResolutionCommand {
                            schema_version: agent_protocol::MCP_INPUT_RESOLUTION_SCHEMA_VERSION,
                            message_id: Uuid::now_v7(),
                            tenant_id: command.tenant_id,
                            run_id,
                            attempt_id,
                            worker_id: command.worker_id,
                            worker_incarnation_id: command.worker_incarnation_id,
                            input_id: resolution.input_id,
                            input_version: resolution.input_version,
                            binding_digest: resolution.binding_digest,
                            responses: resolution.responses,
                            issued_at,
                            expires_at: issued_at + ChronoDuration::minutes(5),
                        },
                        issued_at,
                    )
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                self.emit(run_id, &resolved.event, &mut event_types)?;
                self.persist_checkpoint(run_id, attempt_id)?;
                let started = self
                    .processor
                    .record_mcp_continuation_started(attempt_id, pending.input_id)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                self.emit(run_id, &started, &mut event_types)?;
                self.persist_checkpoint(run_id, attempt_id)?;
                self.execute_started_tool(
                    run_id,
                    attempt_id,
                    resolved.request,
                    Some(resolved.continuation),
                    &mut event_types,
                )
                .await?;
            } else {
                self.pending_mcp_input = Some(pending.clone());
            }
        } else if mcp_resolution.is_some() {
            return Err(LocalRuntimeError::Execution(
                "MCP input response has no matching durable request".into(),
            ));
        }

        if recovery_action == Some(WorkerRecoveryAction::WaitForSubagent) {
            let requests = self
                .processor
                .pending_subagent_requests(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            if requests.is_empty() {
                return Err(LocalRuntimeError::Checkpoint(
                    "subagent recovery action has no pending request".into(),
                ));
            }
            let batch = self
                .run_subagent_batch(run_id, &command.lineage, requests, resolution.take())
                .await?;
            if self.cancellation.is_cancelled() {
                self.terminate_interrupted(run_id, attempt_id, &mut event_types)?;
            } else {
                for result in batch.results {
                    let received = self
                        .processor
                        .record_subagent_result(attempt_id, &result)
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                    self.emit(run_id, &received, &mut event_types)?;
                    self.persist_checkpoint(run_id, attempt_id)?;
                }
                pending_approval = batch.pending_approval;
            }
        } else if let Some(resolution) = resolution.take() {
            if resolution.target_run_id != run_id {
                return Err(LocalRuntimeError::Execution(
                    "approval decision targets another Run".into(),
                ));
            }
            if recovery_action == Some(WorkerRecoveryAction::WaitForApproval) {
                self.answer_pending_approval(run_id, attempt_id, resolution, &mut event_types)
                    .await?;
            } else {
                let already_applied = match (resolution.approval_id, &resolution.binding_digest) {
                    (Some(approval_id), Some(binding_digest)) => self
                        .processor
                        .approval_decision_was_checkpointed(
                            attempt_id,
                            approval_id,
                            binding_digest,
                            match resolution.decision {
                                LocalApprovalDecision::AllowOnce => ToolApprovalDecision::AllowOnce,
                                LocalApprovalDecision::Deny => ToolApprovalDecision::Deny,
                            },
                        )
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?,
                    _ => false,
                };
                if !already_applied {
                    return Err(LocalRuntimeError::Execution(
                        "approval decision has no matching durable Checkpoint receipt".into(),
                    ));
                }
            }
        }

        match recovery_action.clone() {
            Some(WorkerRecoveryAction::RetryToolBatch(requests)) => {
                for request in &requests {
                    let replanned = self
                        .processor
                        .replan_recovered_tool(attempt_id, &request.call.id)
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                    self.emit(run_id, &replanned, &mut event_types)?;
                    let started = self
                        .processor
                        .record_tool_execution_started(
                            attempt_id,
                            &request.call.id,
                            &request.binding_digest,
                        )
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                    self.emit(run_id, &started, &mut event_types)?;
                }
                self.persist_checkpoint(run_id, attempt_id)?;
                self.execute_ordered_tool_batch(run_id, attempt_id, requests, &mut event_types)
                    .await?;
            }
            Some(WorkerRecoveryAction::RetryTool(request)) => {
                let replanned = self
                    .processor
                    .replan_recovered_tool(attempt_id, &request.call.id)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                self.emit(run_id, &replanned, &mut event_types)?;
                self.run_approved_tool(run_id, attempt_id, request, &mut event_types)
                    .await?;
            }
            Some(WorkerRecoveryAction::ResumeMcpTool {
                request,
                pending,
                continuation,
                dispatch_started,
            }) => {
                if !dispatch_started {
                    let started = self
                        .processor
                        .record_mcp_continuation_started(attempt_id, pending.input_id)
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                    self.emit(run_id, &started, &mut event_types)?;
                    self.persist_checkpoint(run_id, attempt_id)?;
                }
                self.execute_started_tool(
                    run_id,
                    attempt_id,
                    request,
                    Some(continuation),
                    &mut event_types,
                )
                .await?;
            }
            Some(WorkerRecoveryAction::TerminateIndeterminate(uncertainty)) => {
                let recovered = if let Some(executor) =
                    self.executors.get(&uncertainty.request.call.name).cloned()
                {
                    let context = ToolExecutionContext {
                        tenant_id: command.tenant_id,
                        application_id: command.application_id,
                        workload_identity_id: command.workload_identity_id,
                        run_id,
                        session_id: command.session_id,
                        workspace_id: command.workspace_id,
                        agent_version_id: command.agent_version_id,
                        attempt_id: uncertainty.source_attempt_id,
                        workspace_root: self.config.workspace_root.clone(),
                        timeout: Duration::from_millis(
                            self.config.runtime_policy.tool_execution.timeout_ms,
                        ),
                        cancellation: self.cancellation.child_token(),
                        requested_at: Utc::now(),
                    };
                    executor
                        .recover_started_result(uncertainty.request.clone(), context)
                        .await
                        .ok()
                        .flatten()
                } else {
                    None
                };
                if let Some(result) = recovered {
                    let recorded = self
                        .processor
                        .record_bound_tool_result(
                            attempt_id,
                            uncertainty.request.call.id,
                            &uncertainty.request.binding_digest,
                            result.content,
                            result.is_error,
                        )
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                    self.emit(run_id, &recorded, &mut event_types)?;
                    checkpoint_path = self.persist_checkpoint(run_id, attempt_id)?;
                } else {
                    let terminal = self
                        .processor
                        .terminate_uncertain_tool(attempt_id)
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                    self.emit(run_id, &terminal, &mut event_types)?;
                    checkpoint_path = self.persist_checkpoint(run_id, attempt_id)?;
                }
            }
            _ => {}
        }

        // A confirmed interrupt may have reached its Checkpoint before the old
        // process stopped the active child. Settle that durable intent before
        // ordinary active tasks are relaunched, or recovery could resume work
        // the caller explicitly redirected.
        if restored_event.is_some() {
            let pending_interrupts = self
                .processor
                .pending_subagent_interrupts(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            for agent_id in pending_interrupts {
                self.settle_pending_subagent_interrupt(
                    run_id,
                    attempt_id,
                    &command.lineage,
                    agent_id,
                    &mut event_types,
                )
                .await?;
            }

            // A confirmed async spawn/send may have reached its Checkpoint
            // before the process launched the in-memory task. Replacement
            // Hosts recreate every unfinished task eagerly; wait remains only
            // an observation API.
            let active = self
                .processor
                .active_subagent_requests(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            for (agent_id, request) in active {
                self.launch_async_subagent(run_id, &command.lineage, agent_id, request);
            }
        }

        if !self
            .processor
            .attempt_is_terminal(attempt_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
        {
            checkpoint_path = self.persist_checkpoint(run_id, attempt_id)?;
        }

        loop {
            if pending_approval.is_some() || self.pending_mcp_input.is_some() {
                break;
            }
            self.reconcile_finished_subagent_tasks(
                run_id,
                attempt_id,
                &command.lineage,
                &mut event_types,
            )
            .await?;
            if self
                .processor
                .attempt_is_terminal(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
            {
                break;
            }
            if self.cancellation.is_cancelled() {
                self.terminate_interrupted(run_id, attempt_id, &mut event_types)?;
                break;
            }
            match self
                .compact_transcript_if_needed(run_id, attempt_id, &mut event_types)
                .await
            {
                Ok(true) => continue,
                Ok(false) => {}
                Err(LocalRuntimeError::ProviderSelection(message)) => {
                    self.terminate_provider_failure(run_id, attempt_id, message, &mut event_types)?;
                    break;
                }
                Err(error) => return Err(error),
            }
            let prepared = self
                .processor
                .prepare_model_invocation(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            let request = decode_model_invocation(&prepared.invocation)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;

            let routed = self
                .execute_model_with_frozen_routing(run_id, attempt_id, &request, &mut event_types)
                .await;
            if self.cancellation.is_cancelled() {
                self.terminate_interrupted(run_id, attempt_id, &mut event_types)?;
                break;
            }
            let (route_journal_path, events) = match routed {
                Ok(routed) => routed,
                Err(LocalRuntimeError::ProviderSelection(message)) => {
                    self.terminate_provider_failure(run_id, attempt_id, message, &mut event_types)?;
                    break;
                }
                Err(error) => return Err(error),
            };

            for event in events {
                match &event {
                    ModelStreamEvent::TextDelta { text } | ModelStreamEvent::Refusal { text } => {
                        output.push_str(text)
                    }
                    _ => {}
                }
                let envelope = self
                    .processor
                    .apply_model_event(attempt_id, event)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                self.emit(run_id, &envelope, &mut event_types)?;
            }

            // The staged response is cleared only after the Worker Checkpoint
            // contains every applied event. A replacement Host can therefore
            // replay the durable staged batch without another Provider call.
            if !self
                .processor
                .attempt_is_terminal(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
            {
                self.persist_checkpoint(run_id, attempt_id)?;
            }
            self.complete_model_route_journal(&route_journal_path)?;

            if self
                .processor
                .attempt_is_terminal(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
            {
                break;
            }
            // Durable before Tool planning so a crash here recovers into
            // "tool calls pending" rather than replaying the model turn.
            self.persist_checkpoint(run_id, attempt_id)?;

            let awaiting = self
                .drain_tool_calls(run_id, attempt_id, &command.lineage, &mut event_types)
                .await?;
            if self
                .processor
                .attempt_is_terminal(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
            {
                break;
            }
            if let Some(approval) = awaiting {
                pending_approval = Some(approval);
                break;
            }
            if self.pending_mcp_input.is_some() {
                break;
            }
            checkpoint_path = self.persist_checkpoint(run_id, attempt_id)?;
        }

        if pending_approval.is_some() || self.pending_mcp_input.is_some() {
            // The pending approval and the stopped clock become durable in the
            // same Checkpoint. Operator think time is therefore excluded even
            // when the daemon restarts before the decision arrives.
            self.processor
                .pause_duration_budget(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            checkpoint_path = self.persist_checkpoint(run_id, attempt_id)?;
        }

        let status = self
            .processor
            .status(attempt_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        if status.is_terminal() {
            self.stop_subagent_tasks().await;
        }
        Ok(LocalRunOutcome {
            run_id,
            attempt_id,
            status,
            event_types,
            output,
            checkpoint_path,
            pending_approval,
            pending_mcp_input: self.pending_mcp_input.clone(),
            mcp_servers: mcp_statuses,
            history_repair: self
                .processor
                .history_repair_report(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?,
        })
    }

    /// Rebinds the Checkpoint's pending approval onto the restored attempt and
    /// applies the operator's decision. Rebinding first is required: the
    /// approval was issued against the attempt that has since been replaced.
    async fn answer_pending_approval(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        resolution: LocalApprovalResolution,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        if resolution.target_run_id != run_id {
            return Err(LocalRuntimeError::Execution(
                "approval decision targets another Run".into(),
            ));
        }
        let rebound = self
            .processor
            .rebind_recovered_approval(attempt_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let approval_id = rebound
            .payload
            .get("approval")
            .and_then(|approval| approval.get("approval_id"))
            .and_then(serde_json::Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| {
                LocalRuntimeError::Execution("rebound approval has no approval id".into())
            })?;
        let binding_digest = rebound
            .payload
            .get("approval")
            .and_then(|approval| approval.get("execution"))
            .and_then(|execution| execution.get("binding_digest"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                LocalRuntimeError::Execution("rebound approval has no binding digest".into())
            })?
            .to_owned();
        if resolution
            .approval_id
            .is_some_and(|expected| expected != approval_id)
            || resolution
                .binding_digest
                .as_ref()
                .is_some_and(|expected| expected != &binding_digest)
        {
            return Err(LocalRuntimeError::Execution(
                "approval decision does not match the recovered Tool binding".into(),
            ));
        }
        self.emit(run_id, &rebound, emitted)?;

        let issued_at = Utc::now();
        let outcome = self
            .processor
            .apply_tool_approval(
                ToolApprovalDecisionCommand {
                    schema_version: TOOL_APPROVAL_DECISION_SCHEMA_VERSION,
                    message_id: Uuid::now_v7(),
                    tenant_id: self.invocation.tenant_id,
                    run_id,
                    attempt_id,
                    worker_id: self.worker_id,
                    worker_incarnation_id: self.worker_id,
                    approval_id,
                    approval_version: 2,
                    binding_digest,
                    decision: match resolution.decision {
                        LocalApprovalDecision::AllowOnce => ToolApprovalDecision::AllowOnce,
                        LocalApprovalDecision::Deny => ToolApprovalDecision::Deny,
                    },
                    issued_at,
                    expires_at: issued_at + ChronoDuration::minutes(5),
                },
                issued_at,
            )
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        for event in &outcome.events {
            self.emit(run_id, event, emitted)?;
        }
        if let Some(request) = outcome.execution {
            self.run_approved_tool(run_id, attempt_id, request, emitted)
                .await?;
        }
        Ok(())
    }

    /// Plans and runs every Tool call the model produced. Returns the emitted
    /// event types and, when consent is `Ask`, the approval that stopped it.
    async fn drain_tool_calls(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        lineage: &AgentLineage,
        emitted: &mut Vec<String>,
    ) -> Result<Option<LocalPendingApproval>, LocalRuntimeError> {
        loop {
            if self
                .processor
                .attempt_is_terminal(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
            {
                break;
            }
            if self
                .processor
                .next_pending_tool_is_subagent(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
            {
                let mut requests = Vec::new();
                while self
                    .processor
                    .next_pending_tool_is_subagent(attempt_id)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
                {
                    let planned = self
                        .processor
                        .plan_next_tool_call(attempt_id)
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                    self.emit(run_id, &planned.event, emitted)?;
                    if let Some(followup) = &planned.followup_event {
                        self.emit(run_id, followup, emitted)?;
                    }
                    match planned.plan {
                        ToolPlan::SubagentSpawn(request) => requests.push(request),
                        _ => {
                            return Err(LocalRuntimeError::Execution(
                                "subagent batch planner returned a non-subagent Tool plan".into(),
                            ));
                        }
                    }
                }
                // Persist the complete ordered batch before any child starts.
                // Recovery can therefore relaunch only the exact owed
                // delegations and reuse any child result receipts already on
                // disk.
                self.persist_checkpoint(run_id, attempt_id)?;
                let (async_requests, inline_requests): (Vec<_>, Vec<_>) = requests
                    .into_iter()
                    .partition(|request| request.mode == SubagentSpawnMode::Async);
                if !async_requests.is_empty() && !inline_requests.is_empty() {
                    return Err(LocalRuntimeError::Execution(
                        "one model turn cannot mix inline and asynchronous subagent spawns".into(),
                    ));
                }
                for request in async_requests {
                    let spawned = self
                        .processor
                        .record_subagent_spawned(attempt_id, &request)
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                    self.emit(run_id, &spawned, emitted)?;
                    // The handle is authoritative only after the parent
                    // Checkpoint contains it. A crash before the task starts is
                    // recovered lazily by agent.wait from the same request.
                    self.persist_checkpoint(run_id, attempt_id)?;
                    let agent_id = request.delegation_id;
                    self.launch_async_subagent(run_id, lineage, agent_id, request);
                }
                if inline_requests.is_empty() {
                    continue;
                }
                let batch = self
                    .run_subagent_batch(run_id, lineage, inline_requests, None)
                    .await?;
                if self.cancellation.is_cancelled() {
                    self.terminate_interrupted(run_id, attempt_id, emitted)?;
                    return Ok(None);
                }
                for result in batch.results {
                    let received = self
                        .processor
                        .record_subagent_result(attempt_id, &result)
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                    self.emit(run_id, &received, emitted)?;
                    self.persist_checkpoint(run_id, attempt_id)?;
                }
                if let Some(approval) = batch.pending_approval {
                    return Ok(Some(approval));
                }
                continue;
            }
            if self.config.consent == LocalToolConsent::AllowOnce {
                let limit = usize::from(
                    self.config
                        .runtime_policy
                        .tool_execution
                        .max_concurrent_tools,
                );
                let prefix = self
                    .processor
                    .pending_parallel_safe_tool_prefix_len(attempt_id, limit)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                if prefix >= 2 {
                    self.run_ordered_parallel_tool_batch(run_id, attempt_id, prefix, emitted)
                        .await?;
                    continue;
                }
            }
            let planned = match self.processor.plan_next_tool_call(attempt_id) {
                Ok(planned) => planned,
                Err(agent_runtime_worker::WorkerAssignmentError::NoPendingToolCall) => break,
                Err(error) => return Err(LocalRuntimeError::Execution(error.to_string())),
            };
            self.emit(run_id, &planned.event, emitted)?;
            if let Some(followup) = &planned.followup_event {
                self.emit(run_id, followup, emitted)?;
            }
            let execution = match planned.plan {
                ToolPlan::Execute(request) => Some(request),
                // Runs, like Execute, but arrived here through an exemption the
                // kernel already recorded as its own durable event.
                ToolPlan::AutoApproved { execution, .. } => Some(execution),
                ToolPlan::ApprovalRequired(approval) => {
                    if self.config.consent == LocalToolConsent::Ask {
                        return Ok(Some(LocalPendingApproval {
                            target_run_id: run_id,
                            approval_id: approval.approval_id,
                            binding_digest: approval.execution.binding_digest.clone(),
                        }));
                    }
                    let issued_at = Utc::now();
                    let outcome = self
                        .processor
                        .apply_tool_approval(
                            ToolApprovalDecisionCommand {
                                schema_version: TOOL_APPROVAL_DECISION_SCHEMA_VERSION,
                                message_id: Uuid::now_v7(),
                                tenant_id: self.invocation.tenant_id,
                                run_id,
                                attempt_id,
                                worker_id: self.worker_id,
                                worker_incarnation_id: self.worker_id,
                                approval_id: approval.approval_id,
                                approval_version: 2,
                                binding_digest: approval.execution.binding_digest.clone(),
                                decision: ToolApprovalDecision::AllowOnce,
                                issued_at,
                                expires_at: issued_at + ChronoDuration::minutes(5),
                            },
                            issued_at,
                        )
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                    for event in &outcome.events {
                        self.emit(run_id, event, emitted)?;
                    }
                    outcome.execution
                }
                ToolPlan::Denied(_) => None,
                ToolPlan::SubagentSpawn(_) => {
                    return Err(LocalRuntimeError::Execution(
                        "subagent Tool escaped the batch planner".into(),
                    ));
                }
            };
            let Some(request) = execution else {
                continue;
            };
            match request.call.name.as_str() {
                "agent.wait" => {
                    self.run_subagent_wait(run_id, attempt_id, lineage, request, emitted)
                        .await?;
                }
                "agent.close" => {
                    self.run_subagent_close(run_id, attempt_id, lineage, request, emitted)
                        .await?;
                }
                "agent.send" => {
                    self.run_subagent_send(run_id, attempt_id, lineage, request, emitted)
                        .await?;
                }
                "agent.history" => {
                    self.run_subagent_history(run_id, attempt_id, request, emitted)?;
                }
                "agent.fork" => {
                    self.run_subagent_fork(run_id, attempt_id, request, emitted)?;
                }
                "agent.rollback" => {
                    self.run_subagent_rollback(run_id, attempt_id, request, emitted)?;
                }
                _ => {
                    self.run_approved_tool(run_id, attempt_id, request, emitted)
                        .await?;
                }
            }
        }
        Ok(None)
    }

    /// Performs policy/approval preflight in source order, then overlaps only
    /// replay-safe `Pure` execution. Results may finish in any order; the
    /// Worker core releases only the contiguous source-order prefix.
    async fn run_ordered_parallel_tool_batch(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        batch_size: usize,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let mut requests = Vec::with_capacity(batch_size);
        for _ in 0..batch_size {
            let planned = self
                .processor
                .plan_next_tool_call(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            self.emit(run_id, &planned.event, emitted)?;
            if let Some(followup) = &planned.followup_event {
                self.emit(run_id, followup, emitted)?;
            }
            let request = match planned.plan {
                ToolPlan::Execute(request) => request,
                ToolPlan::AutoApproved { execution, .. } => execution,
                ToolPlan::ApprovalRequired(approval) => {
                    let issued_at = Utc::now();
                    let outcome = self
                        .processor
                        .apply_tool_approval(
                            ToolApprovalDecisionCommand {
                                schema_version: TOOL_APPROVAL_DECISION_SCHEMA_VERSION,
                                message_id: Uuid::now_v7(),
                                tenant_id: self.invocation.tenant_id,
                                run_id,
                                attempt_id,
                                worker_id: self.worker_id,
                                worker_incarnation_id: self.worker_id,
                                approval_id: approval.approval_id,
                                approval_version: 2,
                                binding_digest: approval.execution.binding_digest.clone(),
                                decision: ToolApprovalDecision::AllowOnce,
                                issued_at,
                                expires_at: issued_at + ChronoDuration::minutes(5),
                            },
                            issued_at,
                        )
                        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                    for event in &outcome.events {
                        self.emit(run_id, event, emitted)?;
                    }
                    outcome.execution.ok_or_else(|| {
                        LocalRuntimeError::Execution(
                            "parallel Tool approval did not produce an execution".into(),
                        )
                    })?
                }
                ToolPlan::Denied(_) | ToolPlan::SubagentSpawn(_) => {
                    return Err(LocalRuntimeError::Execution(
                        "parallel-safe Tool prefix changed during preflight".into(),
                    ));
                }
            };
            requests.push(request);
        }
        self.processor
            .begin_ordered_tool_batch(attempt_id, &requests)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        for request in &requests {
            let started = self
                .processor
                .record_tool_execution_started(
                    attempt_id,
                    &request.call.id,
                    &request.binding_digest,
                )
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            self.emit(run_id, &started, emitted)?;
        }
        // Every start is durable before process launch. A replacement can
        // replay Pure calls and retain any later result that already finished.
        self.persist_checkpoint(run_id, attempt_id)?;
        self.execute_ordered_tool_batch(run_id, attempt_id, requests, emitted)
            .await
    }

    async fn execute_ordered_tool_batch(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        requests: Vec<agent_protocol::ToolExecutionRequest>,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        type PendingToolExecution = Pin<
            Box<
                dyn Future<
                        Output = (
                            agent_protocol::ToolExecutionRequest,
                            Result<ToolExecutionResult, ToolExecutionError>,
                        ),
                    > + Send,
            >,
        >;
        let mut pending = FuturesUnordered::<PendingToolExecution>::new();
        for request in requests {
            let executor: Arc<dyn ToolExecutor> = if let Some(federated) = self
                .processor
                .federated_executor(attempt_id, &request.call.name)
            {
                federated
            } else {
                self.executors
                    .get(&request.call.name)
                    .cloned()
                    .ok_or_else(|| {
                        LocalRuntimeError::ToolExecution(format!(
                            "no tool executor is installed for {}",
                            request.call.name
                        ))
                    })?
            };
            let context = ToolExecutionContext {
                tenant_id: self.invocation.tenant_id,
                application_id: self.invocation.application_id,
                workload_identity_id: self.invocation.workload_identity_id,
                run_id,
                session_id: self
                    .processor
                    .execution_session_id(attempt_id)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?,
                workspace_id: self.invocation.workspace_id,
                agent_version_id: self.invocation.agent_version_id,
                attempt_id,
                workspace_root: self.config.workspace_root.clone(),
                timeout: Duration::from_millis(
                    self.config.runtime_policy.tool_execution.timeout_ms,
                ),
                cancellation: self.cancellation.child_token(),
                requested_at: Utc::now(),
            };
            pending.push(Box::pin(async move {
                let result = executor.execute(request.clone(), context).await;
                (request, result)
            }));
        }

        let mut cancelled = false;
        while let Some((request, result)) = pending.next().await {
            let (content, is_error) = match result {
                Ok(result) => (result.content, result.is_error),
                Err(ToolExecutionError::Cancelled) if self.cancellation.is_cancelled() => {
                    cancelled = true;
                    continue;
                }
                Err(error) => error
                    .deterministic_failure_result()
                    .map(|result| (result.content, result.is_error))
                    .unwrap_or_else(|| {
                        (
                            serde_json::json!({
                                "error": {
                                    "code": "tool_execution_failed",
                                    "message": "tool execution failed inside its assigned sandbox"
                                }
                            }),
                            true,
                        )
                    }),
            };
            let committed = self
                .processor
                .record_bound_tool_result_ordered(
                    attempt_id,
                    request.call.id,
                    &request.binding_digest,
                    content,
                    is_error,
                )
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            for event in &committed {
                self.emit(run_id, event, emitted)?;
            }
            // This also persists a staged out-of-order result even when no
            // event was released yet.
            self.persist_checkpoint(run_id, attempt_id)?;
        }
        if cancelled || self.cancellation.is_cancelled() {
            self.terminate_interrupted(run_id, attempt_id, emitted)?;
        }
        Ok(())
    }

    fn subagent_resolution_owner(
        &self,
        parent_run_id: Uuid,
        requests: &[agent_protocol::SubagentSpawnRequest],
        target_run_id: Uuid,
    ) -> Result<Option<Uuid>, LocalRuntimeError> {
        Self::subagent_resolution_owner_in_state_root(
            &self.config.state_root,
            parent_run_id,
            requests,
            target_run_id,
        )
    }

    fn subagent_resolution_owner_in_state_root(
        state_root: &Path,
        parent_run_id: Uuid,
        requests: &[agent_protocol::SubagentSpawnRequest],
        target_run_id: Uuid,
    ) -> Result<Option<Uuid>, LocalRuntimeError> {
        let mut cursor = target_run_id;
        for _ in 0..=3 {
            if let Some(request) = requests
                .iter()
                .find(|request| request.delegation_id == cursor)
            {
                return Ok(Some(request.delegation_id));
            }
            if cursor == parent_run_id {
                return Ok(None);
            }
            let checkpoint_path = Self::checkpoint_path(state_root, cursor);
            if !checkpoint_path.is_file() {
                return Ok(None);
            }
            let snapshot = Self::load_checkpoint(&checkpoint_path)?;
            let state: serde_json::Value = serde_json::from_slice(&snapshot.state)
                .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
            let Some(parent) = state
                .get("lineage")
                .and_then(|lineage| lineage.get("parent_run_id"))
                .and_then(serde_json::Value::as_str)
                .and_then(|parent| Uuid::parse_str(parent).ok())
            else {
                return Ok(None);
            };
            cursor = parent;
        }
        Ok(None)
    }

    fn launch_async_subagent(
        &mut self,
        parent_run_id: Uuid,
        parent_lineage: &AgentLineage,
        agent_id: Uuid,
        request: agent_protocol::SubagentSpawnRequest,
    ) {
        if self.subagent_tasks.contains_key(&agent_id) {
            return;
        }
        let cancellation = self.cancellation.child_token();
        let task = tokio::spawn(Self::execute_subagent(
            self.config.clone(),
            self.invocation,
            cancellation.clone(),
            parent_run_id,
            parent_lineage.clone(),
            request,
            None,
        ));
        self.subagent_tasks
            .insert(agent_id, LocalSubagentTask { cancellation, task });
    }

    async fn reconcile_finished_subagent_tasks(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        lineage: &AgentLineage,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let finished = self
            .subagent_tasks
            .iter()
            .filter_map(|(agent_id, task)| task.task.is_finished().then_some(*agent_id))
            .collect::<Vec<_>>();
        for agent_id in finished {
            let task = self.subagent_tasks.remove(&agent_id).ok_or_else(|| {
                LocalRuntimeError::Execution("finished subagent task disappeared".into())
            })?;
            let progress = task.task.await.map_err(|error| {
                LocalRuntimeError::Execution(format!("subagent task failed: {error}"))
            })??;
            let LocalSubagentProgress::Completed(result) = progress else {
                // Approval remains authoritative in the child Checkpoint. A
                // later routed decision or wait recreates the task with that
                // exact state; queued input must not bypass the approval.
                continue;
            };
            let terminal = self
                .processor
                .record_async_subagent_result(attempt_id, agent_id, &result)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            self.emit(run_id, &terminal, emitted)?;
            if let Some(activation) = self
                .processor
                .activate_next_subagent_message(attempt_id, agent_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
            {
                self.emit(run_id, &activation.event, emitted)?;
                self.persist_checkpoint(run_id, attempt_id)?;
                self.launch_async_subagent(run_id, lineage, agent_id, activation.request);
            } else {
                self.persist_checkpoint(run_id, attempt_id)?;
            }
        }
        Ok(())
    }

    async fn settle_pending_subagent_interrupt(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        lineage: &AgentLineage,
        agent_id: Uuid,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        // Acceptance and its Tool result are already checkpointed before this
        // phase starts. Yield once at the durable phase boundary so daemon
        // cancellation and event observers can run before the destructive
        // child cancellation begins; recovery owns the same pending intent.
        tokio::task::yield_now().await;
        let active = self
            .processor
            .active_subagent_request(attempt_id, agent_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
            .ok_or_else(|| {
                LocalRuntimeError::Checkpoint(
                    "pending subagent interrupt has no active child turn".into(),
                )
            })?;
        self.launch_async_subagent(run_id, lineage, agent_id, active);
        let mut task = self.subagent_tasks.remove(&agent_id).ok_or_else(|| {
            LocalRuntimeError::Execution("active subagent has no interruptible task".into())
        })?;
        task.cancellation.cancel();
        let timeout = Duration::from_millis(self.config.runtime_policy.tool_execution.timeout_ms);
        let progress = match tokio::time::timeout(timeout, &mut task.task).await {
            Ok(joined) => joined.map_err(|error| {
                LocalRuntimeError::Execution(format!("subagent task failed: {error}"))
            })??,
            Err(_) => {
                task.task.abort();
                let _ = task.task.await;
                return Err(LocalRuntimeError::Execution(
                    "subagent did not stop within the interrupt deadline".into(),
                ));
            }
        };
        let LocalSubagentProgress::Completed(result) = progress else {
            return Err(LocalRuntimeError::Execution(
                "interrupted subagent remained parked on approval".into(),
            ));
        };
        let terminal = self
            .processor
            .record_async_subagent_result(attempt_id, agent_id, &result)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let activation = self
            .processor
            .activate_next_subagent_message(attempt_id, agent_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
            .ok_or_else(|| {
                LocalRuntimeError::Checkpoint(
                    "interrupted subagent has no durable replacement message".into(),
                )
            })?;
        if !activation.receipt.interrupt {
            return Err(LocalRuntimeError::Checkpoint(
                "subagent interrupt did not lead the durable mailbox".into(),
            ));
        }
        self.emit(run_id, &terminal, emitted)?;
        self.emit(run_id, &activation.event, emitted)?;
        self.persist_checkpoint(run_id, attempt_id)?;
        self.launch_async_subagent(run_id, lineage, agent_id, activation.request);
        Ok(())
    }

    async fn run_subagent_wait(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        lineage: &AgentLineage,
        request: agent_protocol::ToolExecutionRequest,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let arguments: LocalSubagentWaitArguments =
            serde_json::from_value(request.call.arguments.clone())
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let started = self
            .processor
            .record_tool_execution_started(attempt_id, &request.call.id, &request.binding_digest)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &started, emitted)?;

        let completed = self
            .processor
            .completed_subagent_result(attempt_id, arguments.agent_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let content = if let Some(result) = completed {
            serde_json::json!({
                "agent_id": arguments.agent_id,
                "timed_out": false,
                "status": result.terminal_status.as_str(),
                "result": result.content
            })
        } else {
            let active = self
                .processor
                .active_subagent_request(attempt_id, arguments.agent_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
                .ok_or_else(|| {
                    LocalRuntimeError::Execution("agent.wait targets an unknown handle".into())
                })?;
            self.launch_async_subagent(run_id, lineage, arguments.agent_id, active);
            let mut task = self
                .subagent_tasks
                .remove(&arguments.agent_id)
                .ok_or_else(|| {
                    LocalRuntimeError::Execution("active subagent has no runnable task".into())
                })?;
            match tokio::time::timeout(Duration::from_millis(arguments.timeout_ms), &mut task.task)
                .await
            {
                Err(_) => {
                    self.subagent_tasks.insert(arguments.agent_id, task);
                    serde_json::json!({
                        "agent_id": arguments.agent_id,
                        "timed_out": true,
                        "status": "running"
                    })
                }
                Ok(joined) => match joined.map_err(|error| {
                    LocalRuntimeError::Execution(format!("subagent task failed: {error}"))
                })?? {
                    LocalSubagentProgress::Completed(result) => {
                        let terminal = self
                            .processor
                            .record_async_subagent_result(attempt_id, arguments.agent_id, &result)
                            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                        self.emit(run_id, &terminal, emitted)?;
                        if let Some(activation) = self
                            .processor
                            .activate_next_subagent_message(attempt_id, arguments.agent_id)
                            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
                        {
                            self.emit(run_id, &activation.event, emitted)?;
                            self.persist_checkpoint(run_id, attempt_id)?;
                            self.launch_async_subagent(
                                run_id,
                                lineage,
                                arguments.agent_id,
                                activation.request,
                            );
                            serde_json::json!({
                                "agent_id": arguments.agent_id,
                                "timed_out": false,
                                "status": "running",
                                "completed_result": result.content,
                                "active_message_sequence": activation.receipt.message_sequence
                            })
                        } else {
                            serde_json::json!({
                                "agent_id": arguments.agent_id,
                                "timed_out": false,
                                "status": result.terminal_status.as_str(),
                                "result": result.content
                            })
                        }
                    }
                    LocalSubagentProgress::AwaitingApproval(approval) => serde_json::json!({
                        "agent_id": arguments.agent_id,
                        "timed_out": false,
                        "status": "waiting_approval",
                        "approval": {
                            "target_run_id": approval.target_run_id,
                            "approval_id": approval.approval_id,
                            "binding_digest": approval.binding_digest
                        }
                    }),
                },
            }
        };
        let recorded = self
            .processor
            .record_bound_tool_result(
                attempt_id,
                request.call.id,
                &request.binding_digest,
                content,
                false,
            )
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &recorded, emitted)?;
        Ok(())
    }

    async fn run_subagent_close(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        lineage: &AgentLineage,
        request: agent_protocol::ToolExecutionRequest,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let arguments: LocalSubagentCloseArguments =
            serde_json::from_value(request.call.arguments.clone())
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let started = self
            .processor
            .record_tool_execution_started(attempt_id, &request.call.id, &request.binding_digest)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &started, emitted)?;

        let completed = self
            .processor
            .completed_subagent_result(attempt_id, arguments.agent_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let content = if let Some(result) = completed {
            serde_json::json!({
                "agent_id": arguments.agent_id,
                "previous_status": result.terminal_status.as_str(),
                "status": result.terminal_status.as_str(),
                "already_terminal": true
            })
        } else {
            let active = self
                .processor
                .active_subagent_request(attempt_id, arguments.agent_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
                .ok_or_else(|| {
                    LocalRuntimeError::Execution("agent.close targets an unknown handle".into())
                })?;
            self.launch_async_subagent(run_id, lineage, arguments.agent_id, active);
            let mut task = self
                .subagent_tasks
                .remove(&arguments.agent_id)
                .ok_or_else(|| {
                    LocalRuntimeError::Execution("active subagent has no cancellable task".into())
                })?;
            task.cancellation.cancel();
            let timeout =
                Duration::from_millis(self.config.runtime_policy.tool_execution.timeout_ms);
            let progress = match tokio::time::timeout(timeout, &mut task.task).await {
                Ok(joined) => joined.map_err(|error| {
                    LocalRuntimeError::Execution(format!("subagent task failed: {error}"))
                })??,
                Err(_) => {
                    task.task.abort();
                    let _ = task.task.await;
                    return Err(LocalRuntimeError::Execution(
                        "subagent did not stop within the close deadline".into(),
                    ));
                }
            };
            let LocalSubagentProgress::Completed(result) = progress else {
                return Err(LocalRuntimeError::Execution(
                    "cancelled subagent remained parked on approval".into(),
                ));
            };
            let terminal = self
                .processor
                .record_async_subagent_result(attempt_id, arguments.agent_id, &result)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
            self.emit(run_id, &terminal, emitted)?;
            serde_json::json!({
                "agent_id": arguments.agent_id,
                "previous_status": "running",
                "status": result.terminal_status.as_str(),
                "already_terminal": false
            })
        };
        if let Some(closed) = self
            .processor
            .record_async_subagent_closed(attempt_id, arguments.agent_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?
        {
            self.emit(run_id, &closed, emitted)?;
        }
        let recorded = self
            .processor
            .record_bound_tool_result(
                attempt_id,
                request.call.id,
                &request.binding_digest,
                content,
                false,
            )
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &recorded, emitted)?;
        Ok(())
    }

    fn run_subagent_history(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        request: agent_protocol::ToolExecutionRequest,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let arguments: LocalSubagentHistoryArguments =
            serde_json::from_value(request.call.arguments.clone())
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let started = self
            .processor
            .record_tool_execution_started(attempt_id, &request.call.id, &request.binding_digest)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &started, emitted)?;
        let history = if let Some(generation) = arguments.generation {
            self.processor.subagent_history_at_generation(
                attempt_id,
                arguments.agent_id,
                generation,
                arguments.after_activation_ordinal,
                arguments.limit,
            )
        } else {
            self.processor.subagent_history(
                attempt_id,
                arguments.agent_id,
                arguments.after_activation_ordinal,
                arguments.limit,
            )
        }
        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let content = serde_json::to_value(history)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let recorded = self
            .processor
            .record_bound_tool_result(
                attempt_id,
                request.call.id,
                &request.binding_digest,
                content,
                false,
            )
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &recorded, emitted)?;
        Ok(())
    }

    fn run_subagent_fork(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        request: agent_protocol::ToolExecutionRequest,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let started = self
            .processor
            .record_tool_execution_started(attempt_id, &request.call.id, &request.binding_digest)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &started, emitted)?;
        let fork = self
            .processor
            .fork_async_subagent(attempt_id, &request.call.id, &request.binding_digest)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        // The new handle and its provenance are durable before the Tool result
        // can make the model act on that handle.
        self.persist_checkpoint(run_id, attempt_id)?;
        let content = serde_json::json!({
            "agent_id": fork.receipt.agent_id,
            "generation": fork.receipt.generation,
            "source_agent_id": fork.receipt.source_agent_id,
            "source_generation": fork.receipt.source_generation,
            "through_activation_ordinal": fork.receipt.through_activation_ordinal,
            "source_history_digest": fork.receipt.source_history_digest,
            "role": fork.receipt.role,
            "budget": fork.receipt.budget
        });
        let recorded = self
            .processor
            .record_bound_tool_result(
                attempt_id,
                request.call.id,
                &request.binding_digest,
                content,
                false,
            )
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.persist_checkpoint(run_id, attempt_id)?;
        self.emit(run_id, &fork.event, emitted)?;
        self.emit(run_id, &recorded, emitted)?;
        Ok(())
    }

    fn run_subagent_rollback(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        request: agent_protocol::ToolExecutionRequest,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let arguments: LocalSubagentRollbackArguments =
            serde_json::from_value(request.call.arguments.clone())
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let started = self
            .processor
            .record_tool_execution_started(attempt_id, &request.call.id, &request.binding_digest)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &started, emitted)?;
        let rollback = self
            .processor
            .rollback_async_subagent(attempt_id, &request.call.id, &request.binding_digest)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        if rollback.receipt.agent_id != arguments.agent_id
            || rollback.receipt.from_generation != arguments.generation
            || rollback.receipt.through_activation_ordinal != arguments.through_activation_ordinal
        {
            return Err(LocalRuntimeError::Checkpoint(
                "subagent rollback receipt changed its requested boundary".into(),
            ));
        }
        // Persist the generation transition before the Tool result can make
        // the model send work to the new head.
        self.persist_checkpoint(run_id, attempt_id)?;
        let content = serde_json::json!({
            "agent_id": rollback.receipt.agent_id,
            "from_generation": rollback.receipt.from_generation,
            "generation": rollback.receipt.generation,
            "through_activation_ordinal": rollback.receipt.through_activation_ordinal,
            "previous_history_digest": rollback.receipt.previous_history_digest,
            "restored_history_digest": rollback.receipt.restored_history_digest
        });
        let recorded = self
            .processor
            .record_bound_tool_result(
                attempt_id,
                request.call.id,
                &request.binding_digest,
                content,
                false,
            )
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.persist_checkpoint(run_id, attempt_id)?;
        self.emit(run_id, &rollback.event, emitted)?;
        self.emit(run_id, &recorded, emitted)?;
        Ok(())
    }

    async fn run_subagent_send(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        lineage: &AgentLineage,
        request: agent_protocol::ToolExecutionRequest,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let arguments: LocalSubagentSendArguments =
            serde_json::from_value(request.call.arguments.clone())
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        let started = self
            .processor
            .record_tool_execution_started(attempt_id, &request.call.id, &request.binding_digest)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &started, emitted)?;

        let continuation = if let Some(generation) = arguments.generation {
            self.processor.continue_async_subagent_at_generation(
                attempt_id,
                arguments.agent_id,
                generation,
                &arguments.idempotency_key,
                &arguments.message,
            )
        } else {
            self.processor.continue_async_subagent(
                attempt_id,
                arguments.agent_id,
                &arguments.idempotency_key,
                &arguments.message,
            )
        }
        .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        if continuation.receipt.interrupt != arguments.interrupt {
            return Err(LocalRuntimeError::Checkpoint(
                "subagent message receipt changed its interrupt intent".into(),
            ));
        }
        let interrupting = continuation.receipt.interrupt
            && continuation.receipt.status == SubagentMessageStatus::Queued;
        let active = continuation.active_request.is_some() || interrupting;
        let status = match continuation.receipt.status {
            SubagentMessageStatus::Queued => "queued",
            SubagentMessageStatus::Active => "accepted",
            SubagentMessageStatus::Completed => "completed",
            SubagentMessageStatus::Cancelled => "cancelled",
        };
        let content = serde_json::json!({
            "agent_id": arguments.agent_id,
            "generation": arguments.generation.unwrap_or(1),
            "submission_id": continuation.receipt.submission_id,
            "idempotency_key": continuation.receipt.idempotency_key,
            "message_sequence": continuation.receipt.message_sequence,
            "status": if interrupting { "accepted" } else { status },
            "active": active,
            "interrupting": interrupting
        });
        let recorded = self
            .processor
            .record_bound_tool_result(
                attempt_id,
                request.call.id,
                &request.binding_digest,
                content,
                false,
            )
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        // The input receipt, successor request and Tool acknowledgement become
        // durable before either the acknowledgement is exposed or a
        // process-local task is allowed to consume it.
        self.persist_checkpoint(run_id, attempt_id)?;
        if let Some(accepted) = continuation.accepted_event {
            self.emit(run_id, &accepted, emitted)?;
        }
        self.emit(run_id, &recorded, emitted)?;
        if interrupting {
            self.settle_pending_subagent_interrupt(
                run_id,
                attempt_id,
                lineage,
                arguments.agent_id,
                emitted,
            )
            .await?;
        } else if let Some(child_request) = continuation.active_request {
            self.launch_async_subagent(run_id, lineage, arguments.agent_id, child_request);
        }
        Ok(())
    }

    async fn run_subagent_batch(
        &self,
        parent_run_id: Uuid,
        parent_lineage: &AgentLineage,
        requests: Vec<agent_protocol::SubagentSpawnRequest>,
        resolution: Option<LocalApprovalResolution>,
    ) -> Result<LocalSubagentBatchProgress, LocalRuntimeError> {
        let resolution_owner = match &resolution {
            Some(resolution) => self
                .subagent_resolution_owner(parent_run_id, &requests, resolution.target_run_id)?
                .ok_or_else(|| {
                    LocalRuntimeError::Execution(
                        "approval decision targets another Run or subagent tree".into(),
                    )
                })?,
            None => Uuid::nil(),
        };
        let mut pending = FuturesUnordered::new();
        for (index, request) in requests.into_iter().enumerate() {
            let child_resolution = resolution
                .as_ref()
                .filter(|_| request.delegation_id == resolution_owner)
                .cloned();
            pending.push(async move {
                (
                    index,
                    self.run_subagent(parent_run_id, parent_lineage, request, child_resolution)
                        .await,
                )
            });
        }
        let mut outcomes = BTreeMap::new();
        while let Some((index, outcome)) = pending.next().await {
            outcomes.insert(index, outcome);
        }
        let mut results = Vec::new();
        let mut pending_approval = None;
        for (_, outcome) in outcomes {
            match outcome? {
                LocalSubagentProgress::Completed(result) => {
                    // A later child may finish while an earlier child is
                    // parked on approval. Its atomic receipt is already safe
                    // on disk, but consuming it now would reverse Tool result
                    // order across the approval restart. Leave that request in
                    // the parent Checkpoint; the next batch pass loads the
                    // receipt and records all results in original call order.
                    if pending_approval.is_none() {
                        results.push(result);
                    }
                }
                LocalSubagentProgress::AwaitingApproval(approval) => {
                    if pending_approval.is_none() {
                        pending_approval = Some(approval);
                    }
                }
            }
        }
        Ok(LocalSubagentBatchProgress {
            results,
            pending_approval,
        })
    }

    async fn run_subagent(
        &self,
        parent_run_id: Uuid,
        parent_lineage: &AgentLineage,
        request: agent_protocol::SubagentSpawnRequest,
        resolution: Option<LocalApprovalResolution>,
    ) -> Result<LocalSubagentProgress, LocalRuntimeError> {
        Self::execute_subagent(
            self.config.clone(),
            self.invocation,
            self.cancellation.child_token(),
            parent_run_id,
            parent_lineage.clone(),
            request,
            resolution,
        )
        .await
    }

    async fn execute_subagent(
        config: LocalRuntimeConfig,
        invocation: RuntimeInvocationContext,
        cancellation: CancellationToken,
        parent_run_id: Uuid,
        parent_lineage: AgentLineage,
        request: agent_protocol::SubagentSpawnRequest,
        resolution: Option<LocalApprovalResolution>,
    ) -> Result<LocalSubagentProgress, LocalRuntimeError> {
        if let Some(result) =
            Self::load_subagent_result(&config.state_root, parent_run_id, &request)?
        {
            return Ok(LocalSubagentProgress::Completed(result));
        }
        if let Some(result) = Self::completed_subagent_result(&config.state_root, &request)? {
            Self::persist_subagent_result(&config.state_root, parent_run_id, &result)?;
            return Ok(LocalSubagentProgress::Completed(result));
        }
        let role = config
            .subagent_roles
            .iter()
            .find(|role| role.name == request.role)
            .cloned()
            .ok_or_else(|| {
                LocalRuntimeError::Execution("subagent role disappeared after planning".into())
            })?;
        let child_depth = parent_lineage.depth.saturating_add(1);
        let child_run_id = request.delegation_id;
        let child_scopes = role.delegated_scopes.clone();
        let child_roles = if child_depth < 3 && child_scopes.contains("agent:spawn") {
            config
                .subagent_roles
                .iter()
                .filter(|candidate| {
                    candidate
                        .delegated_scopes
                        .iter()
                        .all(|scope| child_scopes.contains(scope))
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let child_mcp_servers = config
            .mcp_servers
            .iter()
            .filter(|server| child_scopes.contains(&format!("tool:mcp:{}", server.name)))
            .cloned()
            .collect();
        let mut child_config = config.clone();
        child_config.agent_instructions = role.instructions;
        child_config.delegated_scopes = child_scopes;
        child_config.subagent_roles = child_roles;
        child_config.mcp_servers = child_mcp_servers;
        child_config.budget = request.budget.clone();

        let child_lineage = AgentLineage {
            root_run_id: parent_lineage.root_run_id,
            parent_run_id: Some(parent_run_id),
            delegation_id: Some(request.delegation_id),
            depth: child_depth,
            role: request.role.clone(),
        };
        let mut child = Self::start_for_invocation_with_cancellation(
            child_config,
            invocation,
            cancellation.child_token(),
        )?;
        let checkpoint_path = Self::checkpoint_path(&config.state_root, child_run_id);
        let checkpoint = checkpoint_path
            .is_file()
            .then(|| Self::load_checkpoint(&checkpoint_path))
            .transpose()?;
        let owner_epoch = checkpoint
            .as_ref()
            .map(|snapshot| {
                let state: serde_json::Value = serde_json::from_slice(&snapshot.state)
                    .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
                state
                    .get("owner_epoch")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|epoch| epoch.checked_add(1))
                    .ok_or_else(|| {
                        LocalRuntimeError::Checkpoint(
                            "child checkpoint has no recoverable owner epoch".into(),
                        )
                    })
            })
            .transpose()?
            .unwrap_or(1);
        let child_command = child.local_command_with_lineage_and_history(
            child_run_id,
            &request.input,
            owner_epoch,
            child_lineage,
            request.conversation_history.clone(),
        );
        if let Some(terminal_checkpoint) = checkpoint
            .as_ref()
            .filter(|checkpoint| checkpoint.status.is_terminal())
        {
            let recovered = (|| {
                child
                    .processor
                    .validate_terminal_checkpoint_binding(&child_command, terminal_checkpoint)
                    .map_err(|error| LocalRuntimeError::Checkpoint(error.to_string()))?;
                let child_outcome =
                    child.terminal_local_outcome(child_run_id, terminal_checkpoint)?;
                Self::persist_managed_run_state(
                    &config.state_root,
                    invocation,
                    child_run_id,
                    &request.input,
                    owner_epoch,
                    Self::managed_run_state(&child_outcome),
                )?;
                Self::completed_subagent_result(&config.state_root, &request)?.ok_or_else(|| {
                    LocalRuntimeError::Execution(
                        "terminal child Checkpoint could not publish a durable result".into(),
                    )
                })
            })();
            child.shutdown().await;
            let result = recovered?;
            Self::persist_subagent_result(&config.state_root, parent_run_id, &result)?;
            return Ok(LocalSubagentProgress::Completed(result));
        }
        Self::persist_managed_run_state(
            &config.state_root,
            invocation,
            child_run_id,
            &request.input,
            owner_epoch,
            LocalRunState::Running,
        )?;
        // `drive -> drain_tool_calls -> run_subagent -> drive` is intentionally
        // recursive, bounded by AgentLineage depth. Boxing gives the recursive
        // async state machine a finite representation.
        let child_outcome = Box::pin(child.drive(
            child_command,
            checkpoint,
            resolution.map(LocalResumeResolution::Approval),
        ))
        .await;
        let child_outcome = match child_outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                child.shutdown().await;
                return Err(error);
            }
        };
        Self::persist_managed_run_state(
            &config.state_root,
            invocation,
            child_run_id,
            &request.input,
            owner_epoch,
            Self::managed_run_state(&child_outcome),
        )?;
        if let Some(approval) = child_outcome.pending_approval {
            child.shutdown().await;
            return Ok(LocalSubagentProgress::AwaitingApproval(approval));
        }
        let transcript = child
            .processor
            .conversation_transcript(child_outcome.attempt_id)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()));
        child.shutdown().await;
        let transcript = transcript?;
        let result = Self::completed_subagent_result_with_transcript(
            &config.state_root,
            &request,
            transcript,
        )?
        .ok_or_else(|| {
            LocalRuntimeError::Execution(
                "subagent completed without a durable terminal event identity".into(),
            )
        })?;
        Self::persist_subagent_result(&config.state_root, parent_run_id, &result)?;
        Ok(LocalSubagentProgress::Completed(result))
    }

    /// Executes one authorized Tool call and feeds its bound result back. Shared
    /// by the inline path and the approve-after-restart path so a Tool answered
    /// by a client runs exactly as one answered in-process.
    async fn run_approved_tool(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        request: agent_protocol::ToolExecutionRequest,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let started = self
            .processor
            .record_tool_execution_started(attempt_id, &request.call.id, &request.binding_digest)
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &started, emitted)?;
        // Persist the ambiguity boundary before any side effect can occur.
        // Recovery may replay Pure/Idempotent work, but rejects a started
        // NonIdempotent/Unknown request instead of executing it twice.
        self.persist_checkpoint(run_id, attempt_id)?;

        self.execute_started_tool(run_id, attempt_id, request, None, emitted)
            .await
    }

    async fn execute_started_tool(
        &mut self,
        run_id: Uuid,
        attempt_id: Uuid,
        request: agent_protocol::ToolExecutionRequest,
        continuation: Option<McpInputContinuation>,
        emitted: &mut Vec<String>,
    ) -> Result<(), LocalRuntimeError> {
        let executor: Arc<dyn ToolExecutor> = if let Some(federated) = self
            .processor
            .federated_executor(attempt_id, &request.call.name)
        {
            federated
        } else {
            self.executors
                .get(&request.call.name)
                .cloned()
                .ok_or_else(|| {
                    LocalRuntimeError::ToolExecution(format!(
                        "no tool executor is installed for {}",
                        request.call.name
                    ))
                })?
        };
        let (progress, mut progress_rx) = ToolProgressReporter::channel(32);
        let context = ToolExecutionContext {
            tenant_id: self.invocation.tenant_id,
            application_id: self.invocation.application_id,
            workload_identity_id: self.invocation.workload_identity_id,
            run_id,
            session_id: self
                .processor
                .execution_session_id(attempt_id)
                .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?,
            workspace_id: self.invocation.workspace_id,
            agent_version_id: self.invocation.agent_version_id,
            attempt_id,
            workspace_root: self.config.workspace_root.clone(),
            timeout: Duration::from_millis(self.config.runtime_policy.tool_execution.timeout_ms),
            cancellation: self.cancellation.child_token(),
            requested_at: Utc::now(),
        };
        let execution = if let Some(continuation) = continuation {
            executor.resume_with_mcp_input(request.clone(), context, continuation, progress)
        } else {
            executor.execute_with_progress(request.clone(), context, progress)
        };
        tokio::pin!(execution);
        let result = loop {
            tokio::select! {
                biased;
                result = &mut execution => break result,
                update = progress_rx.recv() => {
                    let Some(update) = update else {
                        continue;
                    };
                    let event = self.processor.record_tool_execution_progress(
                        attempt_id,
                        &request.call.id,
                        &request.binding_digest,
                        update.progress,
                        update.total,
                        update.message.as_deref(),
                    ).map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                    self.emit(run_id, &event, emitted)?;
                    self.persist_checkpoint(run_id, attempt_id)?;
                }
            }
        };
        let result = match result {
            Ok(result) => result,
            Err(ToolExecutionError::Cancelled) if self.cancellation.is_cancelled() => {
                self.terminate_interrupted(run_id, attempt_id, emitted)?;
                return Ok(());
            }
            Err(ToolExecutionError::McpInputRequired {
                round,
                request_state,
                requests,
            }) => {
                let server_name = request
                    .call
                    .name
                    .strip_prefix("mcp:")
                    .and_then(|qualified| qualified.split_once('/'))
                    .map(|(server, _)| server)
                    .ok_or_else(|| {
                        LocalRuntimeError::Execution(
                            "MCP input request came from an unqualified Tool".into(),
                        )
                    })?;
                let server_id = self
                    .config
                    .mcp_servers
                    .iter()
                    .find(|server| server.name == server_name)
                    .map(|server| server.server_id)
                    .ok_or_else(|| {
                        LocalRuntimeError::Execution(
                            "MCP input request came from an unregistered server".into(),
                        )
                    })?;
                let required = self
                    .processor
                    .record_mcp_input_required(
                        attempt_id,
                        &request.call.id,
                        &request.binding_digest,
                        server_id,
                        server_name,
                        round,
                        request_state,
                        requests,
                    )
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                self.emit(run_id, &required.event, emitted)?;
                self.processor
                    .pause_duration_budget(attempt_id)
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                self.persist_checkpoint(run_id, attempt_id)?;
                self.pending_mcp_input = Some(required.pending);
                return Ok(());
            }
            Err(error) => {
                let events = self
                    .processor
                    .record_tool_execution_failure(
                        attempt_id,
                        request.call.id.clone(),
                        &request.binding_digest,
                        &error,
                    )
                    .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
                for event in events {
                    self.emit(run_id, &event, emitted)?;
                }
                // The safe error Tool Result or the indeterminate terminal must
                // be recoverable before the caller observes this Run outcome.
                self.persist_checkpoint(run_id, attempt_id)?;
                return Ok(());
            }
        };
        let recorded = self
            .processor
            .record_bound_tool_result(
                attempt_id,
                request.call.id.clone(),
                &request.binding_digest,
                result.content,
                result.is_error,
            )
            .map_err(|error| LocalRuntimeError::Execution(error.to_string()))?;
        self.emit(run_id, &recorded, emitted)?;
        Ok(())
    }
}
