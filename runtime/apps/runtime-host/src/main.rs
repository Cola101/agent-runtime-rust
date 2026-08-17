//! Local `runtime-host` CLI (ADR-0035).
//!
//! `serve` is the Runtime; `submit` and `attach` are clients of it. A client may
//! be killed at any time without touching a Run, and may reattach later to
//! replay everything from the durable local event log.

use agent_protocol::{RunBudget, SubagentRole};
use agent_runtime_host::embedded::RuntimeControlCommand;
use agent_runtime_host::ipc::{
    LocalRequest, LocalResponse, LocalRuntimeDaemon, default_socket_path,
};
use agent_runtime_host::{
    LocalMcpInputResolution, LocalMcpServerConfig, LocalModelRoutingConfig,
    LocalProcessSessionConfig, LocalProviderConfig, LocalProviderHealthPolicy, LocalRuntimeConfig,
    LocalRuntimeHost, LocalToolConsent, WORKSPACE_READ_SCOPE,
};
use agent_runtime_invocation_protocol::v1::runtime_invocation_server::RuntimeInvocationServer;
use std::collections::BTreeSet;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_stream::wrappers::TcpListenerStream;
use uuid::Uuid;

fn required(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    std::env::var(name).map_err(|_| format!("{name} is required").into())
}

fn state_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(PathBuf::from(required("AGENT_RUNTIME_LOCAL_STATE_ROOT")?))
}

/// Everything the network invocation surface needs, or nothing at all.
struct InvocationSurface {
    bind_address: std::net::SocketAddr,
    tls: agent_grpc_security::ServerMtlsMaterials,
    verifier: agent_workload_identity::WorkloadTokenVerifier,
}

/// Resolves the network surface configuration.
///
/// Two properties, both deliberate:
///
/// - **Off unless asked for.** No bind address means no surface. The local Unix
///   socket stays the only way in, exactly as before, so an existing
///   installation does not silently gain a network listener on upgrade.
/// - **Cannot be turned on without mTLS and a verifying key.** Naming a bind
///   address without the materials is a configuration error that refuses to
///   start -- never a reason to serve in the clear. The Unix socket could treat
///   "you can open this file" as the authorization; a TCP port has no such
///   thing, and a surface that starts Runs must not be the place we discover
///   that.
fn load_invocation_surface() -> Result<Option<InvocationSurface>, Box<dyn std::error::Error>> {
    let Ok(bind_address) = std::env::var("AGENT_RUNTIME_INVOCATION_BIND") else {
        return Ok(None);
    };
    let bind_address = bind_address.parse::<std::net::SocketAddr>().map_err(|_| {
        format!("AGENT_RUNTIME_INVOCATION_BIND is not a socket address: {bind_address}")
    })?;
    let tls = agent_grpc_security::ServerMtlsMaterials::from_files(
        required("AGENT_RUNTIME_GRPC_SERVER_CERT")?,
        required("AGENT_RUNTIME_GRPC_SERVER_KEY")?,
        required("AGENT_RUNTIME_GRPC_CLIENT_CA_CERT")?,
    )?;
    let verifier = agent_workload_identity::WorkloadTokenVerifier::from_base64(&required(
        "AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY",
    )?)?;
    Ok(Some(InvocationSurface {
        bind_address,
        tls,
        verifier,
    }))
}

fn load_mcp_servers_from_path(
    path: &std::path::Path,
) -> Result<Vec<LocalMcpServerConfig>, Box<dyn std::error::Error>> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err("local MCP config must be a regular JSON file no larger than 1 MiB".into());
    }
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(Into::into)
}

fn load_mcp_servers() -> Result<Vec<LocalMcpServerConfig>, Box<dyn std::error::Error>> {
    match std::env::var("AGENT_RUNTIME_LOCAL_MCP_CONFIG") {
        Ok(path) => load_mcp_servers_from_path(std::path::Path::new(&path)),
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn load_subagent_roles_from_path(
    path: &std::path::Path,
) -> Result<Vec<SubagentRole>, Box<dyn std::error::Error>> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(
            "local subagent config must be a regular JSON file no larger than 1 MiB".into(),
        );
    }
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(Into::into)
}

fn load_subagent_roles() -> Result<Vec<SubagentRole>, Box<dyn std::error::Error>> {
    match std::env::var("AGENT_RUNTIME_LOCAL_SUBAGENT_CONFIG") {
        Ok(path) => load_subagent_roles_from_path(std::path::Path::new(&path)),
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalModelRoutingFile {
    candidates: Vec<LocalProviderFile>,
    allowed_regions: BTreeSet<String>,
    data_class: agent_model_gateway::DataClass,
    max_cost_per_million_tokens_micros: u64,
    #[serde(default)]
    health_policy: LocalProviderHealthPolicy,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalProviderFile {
    id: String,
    protocol: agent_model_gateway::ProviderProtocol,
    endpoint: String,
    model: String,
    api_key_env: String,
    region: String,
    accepted_data_classes: BTreeSet<agent_model_gateway::DataClass>,
    capabilities: BTreeSet<agent_model_gateway::Capability>,
    healthy: bool,
    latency_ms: u64,
    cost_per_million_tokens_micros: u64,
    response_timeout_ms: u64,
    stream_idle_timeout_ms: u64,
}

fn load_model_routing_from_path(
    path: &std::path::Path,
    mut read_secret: impl FnMut(&str) -> Result<String, std::env::VarError>,
) -> Result<LocalModelRoutingConfig, Box<dyn std::error::Error>> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(
            "local model routing config must be a regular JSON file no larger than 1 MiB".into(),
        );
    }
    let bytes = std::fs::read(path)?;
    let file: LocalModelRoutingFile = serde_json::from_slice(&bytes)?;
    let candidates = file
        .candidates
        .into_iter()
        .map(|candidate| {
            if candidate.api_key_env.is_empty()
                || candidate.api_key_env.len() > 128
                || !candidate
                    .api_key_env
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                return Err(format!(
                    "provider {} has an invalid api_key_env",
                    candidate.id
                ));
            }
            let api_key = read_secret(&candidate.api_key_env).map_err(|_| {
                format!(
                    "provider {} requires secret environment variable {}",
                    candidate.id, candidate.api_key_env
                )
            })?;
            Ok(LocalProviderConfig {
                id: candidate.id,
                protocol: candidate.protocol,
                endpoint: candidate.endpoint,
                model: candidate.model,
                api_key,
                region: candidate.region,
                accepted_data_classes: candidate.accepted_data_classes,
                capabilities: candidate.capabilities,
                healthy: candidate.healthy,
                latency_ms: candidate.latency_ms,
                cost_per_million_tokens_micros: candidate.cost_per_million_tokens_micros,
                response_timeout_ms: candidate.response_timeout_ms,
                stream_idle_timeout_ms: candidate.stream_idle_timeout_ms,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(LocalModelRoutingConfig {
        candidates,
        allowed_regions: file.allowed_regions,
        data_class: file.data_class,
        max_cost_per_million_tokens_micros: file.max_cost_per_million_tokens_micros,
        health_policy: file.health_policy,
    })
}

fn load_model_routing() -> Result<LocalModelRoutingConfig, Box<dyn std::error::Error>> {
    match std::env::var("AGENT_RUNTIME_LOCAL_MODEL_ROUTING_CONFIG") {
        Ok(path) => {
            load_model_routing_from_path(std::path::Path::new(&path), |name| std::env::var(name))
        }
        Err(std::env::VarError::NotPresent) => {
            Ok(LocalModelRoutingConfig::single_openai_compatible(
                required("AGENT_RUNTIME_LOCAL_PROVIDER_ENDPOINT")?,
                required("AGENT_RUNTIME_LOCAL_PROVIDER_MODEL")?,
                required("AGENT_RUNTIME_LOCAL_PROVIDER_API_KEY")?,
            ))
        }
        Err(error) => Err(error.into()),
    }
}

fn load_config() -> Result<LocalRuntimeConfig, Box<dyn std::error::Error>> {
    let runtime_executable = std::env::current_exe()?;
    let consent = match std::env::var("AGENT_RUNTIME_LOCAL_TOOL_CONSENT").as_deref() {
        Ok("allow-once") => LocalToolConsent::AllowOnce,
        Ok("ask") | Err(_) => LocalToolConsent::Ask,
        Ok(other) => return Err(format!("unsupported tool consent {other}").into()),
    };
    let scopes = std::env::var("AGENT_RUNTIME_LOCAL_DELEGATED_SCOPES")
        .unwrap_or_else(|_| WORKSPACE_READ_SCOPE.to_string())
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    Ok(LocalRuntimeConfig {
        state_root: state_root()?,
        workspace_root: PathBuf::from(required("AGENT_RUNTIME_LOCAL_WORKSPACE_ROOT")?),
        agent_instructions: std::env::var("AGENT_RUNTIME_LOCAL_INSTRUCTIONS").unwrap_or_else(
            |_| "You are a local agent. Explain evidence before conclusions.".into(),
        ),
        delegated_scopes: scopes,
        subagent_roles: load_subagent_roles()?,
        model_routing: load_model_routing()?,
        mcp_servers: load_mcp_servers()?,
        mcp_lifecycle: agent_runtime_host::LocalMcpLifecycleConfig::default(),
        trusted_workspace_tool: std::env::var("AGENT_RUNTIME_LOCAL_TRUSTED_TOOL_BIN")
            .ok()
            .map(PathBuf::from),
        process_session: std::env::var_os("AGENT_RUNTIME_LOCAL_PROCESS_EXECUTABLE").map(
            |executable| LocalProcessSessionConfig {
                executable: PathBuf::from(executable),
                fixed_args: Vec::new(),
                max_output_chunk_bytes: 64 * 1024,
                governance: agent_tool_runtime::ProcessSessionGovernance::default(),
                pty_supervisor: Some(agent_tool_runtime::ProcessSessionPtySupervisorConfig {
                    executable: runtime_executable.clone(),
                    fixed_args: vec!["__pty-session-supervisor".into()],
                    startup_timeout: std::time::Duration::from_secs(10),
                }),
            },
        ),
        consent,
        budget: RunBudget {
            max_tokens: 8_192,
            max_cost_cents: 100,
            max_duration_seconds: 600,
        },
        runtime_policy: agent_protocol::RuntimeExecutionPolicySnapshot::default(),
    })
}

async fn client_request(request: &LocalRequest) -> Result<(), Box<dyn std::error::Error>> {
    let socket = default_socket_path(&state_root()?);
    let stream = UnixStream::connect(&socket)
        .await
        .map_err(|error| format!("no local runtime-host at {}: {error}", socket.display()))?;
    let (reader, mut writer) = stream.into_split();
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        println!("{line}");
        if serde_json::from_str::<LocalResponse>(&line)
            .is_ok_and(|response| matches!(response, LocalResponse::Finished { .. }))
        {
            break;
        }
        if !matches!(request, LocalRequest::Attach { .. }) {
            break;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().json().init();
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "run".into());

    match command.as_str() {
        "__pty-session-supervisor" => {
            if args.next().as_deref() != Some("--state-root") {
                return Err("missing --state-root for PTY session supervisor".into());
            }
            let state_root = PathBuf::from(
                args.next()
                    .ok_or("missing PTY session supervisor state root")?,
            );
            if args.next().is_some() {
                return Err("unexpected PTY session supervisor argument".into());
            }
            agent_tool_runtime::run_process_session_pty_supervisor(state_root).await?;
            Ok(())
        }
        // The long-running Runtime. Runs outlive every client connection.
        "serve" => {
            // Resolved first, before the rest of the configuration and before
            // any recovery. It is cheap and independent, and it is the one
            // security-relevant gate here: "you asked for a network surface
            // without mTLS" must never be masked by an unrelated
            // configuration error that an operator fixes and restarts past.
            let surface = load_invocation_surface()?;
            let config = load_config()?;
            let socket = default_socket_path(&config.state_root);
            let listener = match LocalRuntimeDaemon::bind(&socket).await {
                Ok(listener) => listener,
                // Not a broken installation: a host is already serving this
                // state root, and a client should talk to that one. Said
                // plainly here because a desktop client started twice is the
                // ordinary way to reach this.
                Err(error @ agent_runtime_host::LocalRuntimeError::AlreadyRunning(_)) => {
                    eprintln!("{error}");
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            let daemon = LocalRuntimeDaemon::new(config);
            // Pick up whatever an earlier daemon left unfinished before taking
            // new work, so a restart never strands a Run that has a Checkpoint.
            let resumed = daemon.recover_unfinished().await?;
            eprintln!(
                "runtime-host listening on {} (resumed {resumed} unfinished run(s))",
                socket.display()
            );

            // Both adapters drive the SAME EmbeddedRuntime. A second instance
            // over one state root would give each its own admission ceilings,
            // owner epochs and retention gates while both believed they owned
            // the directory.
            if let Some(surface) = surface {
                let service = agent_runtime_host::grpc::RuntimeInvocationGrpcService::new(
                    daemon.runtime(),
                    surface.verifier,
                );
                let grpc_listener = tokio::net::TcpListener::bind(surface.bind_address).await?;
                // Built before the spawn so a bad TLS configuration fails the
                // process here, rather than inside a task nobody is awaiting.
                let server = tonic::transport::Server::builder()
                    .tls_config(surface.tls.into_tonic())?
                    .add_service(RuntimeInvocationServer::new(service));
                eprintln!(
                    "runtime-host invocation surface listening on {} (mTLS required)",
                    surface.bind_address
                );
                tokio::spawn(async move {
                    if let Err(error) = server
                        .serve_with_incoming(TcpListenerStream::new(grpc_listener))
                        .await
                    {
                        eprintln!("runtime-host invocation surface stopped: {error}");
                    }
                });
            }

            daemon.serve(listener).await;
            Ok(())
        }
        "submit" => {
            let input = args.next().ok_or("usage: runtime-host submit <input>")?;
            client_request(&LocalRequest::Submit { input }).await
        }
        "attach" => {
            let run_id: Uuid = args
                .next()
                .ok_or("usage: runtime-host attach <run-id> [after-sequence]")?
                .parse()?;
            let after_sequence = args.next().unwrap_or_else(|| "0".into()).parse()?;
            client_request(&LocalRequest::Attach {
                run_id,
                after_sequence,
            })
            .await
        }
        "list" => client_request(&LocalRequest::List).await,
        "mcp-input" => {
            let run_id: Uuid = args
                .next()
                .ok_or("usage: runtime-host mcp-input <run-id> <resolution-json>")?
                .parse()?;
            let resolution: LocalMcpInputResolution = serde_json::from_str(
                &args
                    .next()
                    .ok_or("usage: runtime-host mcp-input <run-id> <resolution-json>")?,
            )?;
            client_request(&LocalRequest::ResolveMcpInput {
                run_id,
                input_id: resolution.input_id,
                input_version: resolution.input_version,
                binding_digest: resolution.binding_digest,
                responses: resolution.responses,
            })
            .await
        }
        "approve" | "deny" | "cancel" | "resume" => {
            let run_id: Uuid = args
                .next()
                .ok_or("usage: runtime-host <approve|deny|cancel|resume> <run-id>")?
                .parse()?;
            let request = match command.as_str() {
                "approve" => LocalRequest::Approve { run_id },
                "deny" => LocalRequest::Deny { run_id },
                "cancel" => LocalRequest::Cancel { run_id },
                _ => LocalRequest::Resume { run_id },
            };
            client_request(&request).await
        }
        "control" => {
            let command: RuntimeControlCommand = serde_json::from_str(
                &args
                    .next()
                    .ok_or("usage: runtime-host control <command-json>")?,
            )?;
            client_request(&LocalRequest::Control { command }).await
        }
        // One-shot execution without a daemon, for scripting.
        "run" => {
            let input = args.next().ok_or("usage: runtime-host run <input>")?;
            let mut host = LocalRuntimeHost::start(load_config()?)?;
            let outcome = host.execute(&input).await;
            host.shutdown().await;
            let outcome = outcome?;
            println!(
                "{}",
                serde_json::json!({
                    "run_id": outcome.run_id,
                    "attempt_id": outcome.attempt_id,
                    "status": outcome.status,
                    "events": outcome.event_types,
                    "output": outcome.output,
                    "checkpoint": outcome.checkpoint_path,
                    "pending_approval": outcome.pending_approval,
                    "pending_mcp_input": outcome.pending_mcp_input,
                    "mcp_servers": outcome.mcp_servers,
                })
            );
            Ok(())
        }
        other => Err(format!("unsupported command {other}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_runtime_host::LocalMcpTransportConfig;

    /// The production break this catches is exposing stdio only through the Rust
    /// library while the shipped binary silently constructs an empty MCP list.
    #[test]
    fn local_mcp_config_file_is_consumed_by_the_binary() {
        let root = tempfile::tempdir().expect("temp config root");
        let path = root.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"[{
                "server_id":"00000000-0000-4000-8000-000000000044",
                "name":"local",
                "transport":{
                    "type":"stdio",
                    "command":"/bin/sh",
                    "args":["server.sh"],
                    "env":{"EXPLICIT_VALUE":"present"},
                    "cwd":null
                },
                "tool_names":["search"]
            }]"#,
        )
        .unwrap();

        let servers = load_mcp_servers_from_path(&path).expect("valid MCP config");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "local");
        assert!(matches!(
            &servers[0].transport,
            LocalMcpTransportConfig::Stdio { command, args, env, cwd }
                if command.as_path() == std::path::Path::new("/bin/sh")
                    && args == &["server.sh"]
                    && env.get("EXPLICIT_VALUE").map(String::as_str) == Some("present")
                    && cwd.is_none()
        ));
    }

    #[test]
    fn local_subagent_role_file_is_consumed_by_the_binary() {
        let root = tempfile::tempdir().expect("temp config root");
        let path = root.path().join("subagents.json");
        std::fs::write(
            &path,
            r#"[{
                "name":"reviewer",
                "instructions":"Review evidence only.",
                "delegated_scopes":["tool:workspace.read"]
            }]"#,
        )
        .unwrap();

        let roles = load_subagent_roles_from_path(&path).expect("valid subagent config");
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].name, "reviewer");
        assert_eq!(roles[0].instructions, "Review evidence only.");
        assert_eq!(
            roles[0].delegated_scopes,
            BTreeSet::from(["tool:workspace.read".to_owned()])
        );
    }

    #[test]
    fn local_multi_provider_routing_file_is_consumed_without_embedding_secrets() {
        let root = tempfile::tempdir().expect("temp config root");
        let path = root.path().join("model-routing.json");
        std::fs::write(
            &path,
            r#"{
                "allowed_regions":["us","eu"],
                "data_class":"confidential",
                "max_cost_per_million_tokens_micros":900000,
                "health_policy":{
                    "max_same_provider_attempts":3,
                    "initial_retry_backoff_ms":125,
                    "max_retry_backoff_ms":2000,
                    "consecutive_failure_threshold":2,
                    "cooldown_ms":30000,
                    "max_retry_after_ms":60000,
                    "half_open_probe_lease_ms":120000
                },
                "candidates":[
                    {
                        "id":"responses-primary",
                        "protocol":"open_ai_responses",
                        "endpoint":"https://responses.example.test/v1/responses",
                        "model":"response-model",
                        "api_key_env":"RESPONSES_API_KEY",
                        "region":"us",
                        "accepted_data_classes":["public","internal","confidential"],
                        "capabilities":["text","tool_use","structured_output"],
                        "healthy":true,
                        "latency_ms":20,
                        "cost_per_million_tokens_micros":700000,
                        "response_timeout_ms":45000,
                        "stream_idle_timeout_ms":15000
                    },
                    {
                        "id":"anthropic-fallback",
                        "protocol":"anthropic_messages",
                        "endpoint":"https://anthropic.example.test/v1/messages",
                        "model":"message-model",
                        "api_key_env":"ANTHROPIC_API_KEY",
                        "region":"eu",
                        "accepted_data_classes":["public","internal","confidential"],
                        "capabilities":["text","tool_use"],
                        "healthy":true,
                        "latency_ms":30,
                        "cost_per_million_tokens_micros":800000,
                        "response_timeout_ms":45000,
                        "stream_idle_timeout_ms":15000
                    }
                ]
            }"#,
        )
        .expect("write routing config");

        let mut requested_secrets = Vec::new();
        let routing = load_model_routing_from_path(&path, |name| {
            requested_secrets.push(name.to_owned());
            Ok::<_, std::env::VarError>(format!("secret-for-{name}"))
        })
        .expect("valid routing config");

        assert_eq!(
            requested_secrets,
            ["RESPONSES_API_KEY", "ANTHROPIC_API_KEY"]
        );
        assert_eq!(routing.candidates.len(), 2);
        assert_eq!(
            routing.candidates[0].api_key,
            "secret-for-RESPONSES_API_KEY"
        );
        assert_eq!(
            routing.candidates[1].api_key,
            "secret-for-ANTHROPIC_API_KEY"
        );
        assert_eq!(
            routing.allowed_regions,
            BTreeSet::from(["eu".into(), "us".into()])
        );
        assert_eq!(
            routing.data_class,
            agent_model_gateway::DataClass::Confidential
        );
        assert_eq!(routing.health_policy.max_same_provider_attempts, 3);
        assert_eq!(routing.health_policy.initial_retry_backoff_ms, 125);
        assert_eq!(routing.health_policy.cooldown_ms, 30_000);
    }
}
