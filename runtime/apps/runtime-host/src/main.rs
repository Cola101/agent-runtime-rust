//! Local `runtime-host` CLI (ADR-0035).
//!
//! `serve` is the Runtime; `submit` and `attach` are clients of it. A client may
//! be killed at any time without touching a Run, and may reattach later to
//! replay everything from the durable local event log.

use agent_protocol::RunBudget;
use agent_runtime_host::ipc::{
    LocalRequest, LocalResponse, LocalRuntimeDaemon, default_socket_path,
};
use agent_runtime_host::{
    LocalProviderConfig, LocalRuntimeConfig, LocalRuntimeHost, LocalToolConsent,
    WORKSPACE_READ_SCOPE,
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use uuid::Uuid;

fn required(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    std::env::var(name).map_err(|_| format!("{name} is required").into())
}

fn state_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(PathBuf::from(required("AGENT_RUNTIME_LOCAL_STATE_ROOT")?))
}

fn load_config() -> Result<LocalRuntimeConfig, Box<dyn std::error::Error>> {
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
        provider: LocalProviderConfig {
            endpoint: required("AGENT_RUNTIME_LOCAL_PROVIDER_ENDPOINT")?,
            model: required("AGENT_RUNTIME_LOCAL_PROVIDER_MODEL")?,
            api_key: required("AGENT_RUNTIME_LOCAL_PROVIDER_API_KEY")?,
        },
        trusted_workspace_tool: std::env::var("AGENT_RUNTIME_LOCAL_TRUSTED_TOOL_BIN")
            .ok()
            .map(PathBuf::from),
        consent,
        budget: RunBudget {
            max_tokens: 8_192,
            max_cost_cents: 100,
            max_duration_seconds: 600,
        },
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
        // The long-running Runtime. Runs outlive every client connection.
        "serve" => {
            let config = load_config()?;
            let socket = default_socket_path(&config.state_root);
            let listener = LocalRuntimeDaemon::bind(&socket).await?;
            let daemon = LocalRuntimeDaemon::new(config);
            // Pick up whatever an earlier daemon left unfinished before taking
            // new work, so a restart never strands a Run that has a Checkpoint.
            let resumed = daemon.recover_unfinished().await?;
            eprintln!(
                "runtime-host listening on {} (resumed {resumed} unfinished run(s))",
                socket.display()
            );
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
        "approve" | "deny" | "cancel" => {
            let run_id: Uuid = args
                .next()
                .ok_or("usage: runtime-host <approve|deny|cancel> <run-id>")?
                .parse()?;
            let request = match command.as_str() {
                "approve" => LocalRequest::Approve { run_id },
                "deny" => LocalRequest::Deny { run_id },
                _ => LocalRequest::Cancel { run_id },
            };
            client_request(&request).await
        }
        // One-shot execution without a daemon, for scripting.
        "run" => {
            let input = args.next().ok_or("usage: runtime-host run <input>")?;
            let mut host = LocalRuntimeHost::start(load_config()?)?;
            let outcome = host.execute(&input).await?;
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
                })
            );
            Ok(())
        }
        other => Err(format!("unsupported command {other}").into()),
    }
}
