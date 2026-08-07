use agent_grpc_security::ClientMtlsMaterials;
use agent_nats_security::NatsClientConfig;
use agent_protocol::Placement;
use agent_runtime_health::{HealthState, serve as serve_health};
use agent_runtime_worker::{
    GrpcCheckpointPayloadStore, NatsWorker, SkillArtifactVerifier, WorkerAdmissionFence,
    WorkerPollResult, WorkerProcessor, load_or_create_worker_id, prepare_trusted_workspace_tool,
};
use agent_workload_identity::WorkloadTokenVerifier;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().json().init();
    let health = HealthState::default();
    let health_listener = tokio::net::TcpListener::bind(
        std::env::var("AGENT_RUNTIME_HEALTH_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
    )
    .await?;
    tokio::spawn(serve_health(health_listener, health.clone()));
    let worker_id = match std::env::var("AGENT_RUNTIME_WORKER_ID_FILE") {
        Ok(path) => load_or_create_worker_id(PathBuf::from(path))?,
        Err(_) => Uuid::parse_str(&std::env::var("AGENT_RUNTIME_WORKER_ID")?)?,
    };
    let worker_incarnation_id = Uuid::now_v7();
    let nats = NatsClientConfig::new(
        std::env::var("AGENT_RUNTIME_NATS_URL")?,
        std::env::var("AGENT_RUNTIME_NATS_USERNAME")?,
        std::env::var("AGENT_RUNTIME_NATS_PASSWORD")?,
        PathBuf::from(std::env::var("AGENT_RUNTIME_NATS_CA_CERT")?),
    )?;
    let capacity = std::env::var("AGENT_RUNTIME_WORKER_CAPACITY")
        .unwrap_or_else(|_| "8".to_string())
        .parse::<u32>()?;
    let drain_grace = parse_drain_grace(
        &std::env::var("AGENT_RUNTIME_DRAIN_GRACE_SECONDS").unwrap_or_else(|_| "90".to_string()),
    )?;
    let model_gateway_endpoint = std::env::var("AGENT_RUNTIME_MODEL_GATEWAY_ENDPOINT")?;
    let checkpoint_gateway_endpoint = std::env::var("AGENT_RUNTIME_CHECKPOINT_GATEWAY_ENDPOINT")?;
    let workload_token_verifier = WorkloadTokenVerifier::from_base64(&std::env::var(
        "AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY",
    )?)?;
    let client_cert_path = std::env::var("AGENT_RUNTIME_GRPC_CLIENT_CERT")?;
    let client_key_path = std::env::var("AGENT_RUNTIME_GRPC_CLIENT_KEY")?;
    let server_ca_path = std::env::var("AGENT_RUNTIME_GRPC_SERVER_CA_CERT")?;
    let model_gateway_tls = ClientMtlsMaterials::from_files(
        &client_cert_path,
        &client_key_path,
        &server_ca_path,
        std::env::var("AGENT_RUNTIME_MODEL_GATEWAY_TLS_DOMAIN")?,
    )?;
    let checkpoint_gateway_tls = ClientMtlsMaterials::from_files(
        &client_cert_path,
        &client_key_path,
        &server_ca_path,
        std::env::var("AGENT_RUNTIME_CHECKPOINT_GATEWAY_TLS_DOMAIN")?,
    )?;
    let mut processor = WorkerProcessor::new_with_incarnation(
        worker_id,
        worker_incarnation_id,
        vec![Placement::Cloud],
        capacity,
        env!("CARGO_PKG_VERSION").to_string(),
    )?;
    processor.set_skill_artifact_verifier(SkillArtifactVerifier::from_base64(
        std::env::var("AGENT_RUNTIME_SKILL_SIGNING_KEY_ID")?,
        &std::env::var("AGENT_RUNTIME_SKILL_SIGNING_PUBLIC_KEY")?,
    )?);
    let trusted_tools_enabled = trusted_native_tools_enabled(
        std::env::var("AGENT_RUNTIME_TRUSTED_NATIVE_TOOLS")
            .ok()
            .as_deref(),
    )?;
    let trusted_workspace_tool = if trusted_tools_enabled {
        prepare_trusted_workspace_tool(
            true,
            PathBuf::from(std::env::var("AGENT_RUNTIME_TRUSTED_WORKSPACE_TOOL_BIN")?),
            PathBuf::from(std::env::var("AGENT_RUNTIME_WORKSPACE_ROOT")?),
        )?
    } else {
        None
    };
    if let Some(configured) = &trusted_workspace_tool {
        for tool in &configured.tools {
            processor.register_tool(tool.definition.clone())?;
        }
    }
    let mut worker = NatsWorker::connect_secure_with_model_gateway_mtls(
        &nats,
        processor,
        &model_gateway_endpoint,
        model_gateway_tls,
    )
    .await?;
    worker.set_workload_token_verifier(workload_token_verifier);
    if let Some(configured) = trusted_workspace_tool {
        worker.set_workspace_root(configured.workspace_root);
        for tool in configured.tools {
            worker.register_tool_executor(
                tool.definition.descriptor.name.clone(),
                agent_protocol::SandboxClass::TrustedNative,
                tool.executor,
            )?;
        }
    }
    let checkpoint_store = GrpcCheckpointPayloadStore::connect_with_mtls(
        checkpoint_gateway_endpoint.clone(),
        checkpoint_gateway_tls,
    )
    .await?;
    worker.set_checkpoint_store(Arc::new(checkpoint_store));
    health.set_ready(worker.nats_connection_is_ready());
    info!(
        component = "runtime-worker",
        %worker_id,
        %worker_incarnation_id,
        capacity,
        drain_grace_seconds = drain_grace.as_secs(),
        %model_gateway_endpoint,
        %checkpoint_gateway_endpoint,
        trusted_native_tools = trusted_tools_enabled,
        "worker connected"
    );

    let shutdown = CancellationToken::new();
    let admission_fence = worker.admission_fence();
    let _shutdown_listener =
        install_shutdown_listener(health.clone(), admission_fence, shutdown.clone());
    let mut drain_deadline = None;

    loop {
        if shutdown.is_cancelled() && !worker.is_draining() {
            let draining_since = chrono::Utc::now();
            let deadline = draining_since
                + chrono::Duration::from_std(drain_grace)
                    .expect("bounded drain grace fits chrono duration");
            worker.begin_draining(draining_since, deadline)?;
            drain_deadline = Some(tokio::time::Instant::now() + drain_grace);
            health.set_ready(false);
            worker.publish_heartbeat().await?;
            info!(
                %worker_id,
                active_attempts = worker.active_attempt_count(),
                drain_deadline = %deadline,
                "worker admission closed; draining active attempts"
            );
        }
        health.set_ready(worker.is_accepting_work() && worker.nats_connection_is_ready());
        worker.publish_heartbeat().await?;
        match worker
            .poll_identity_renewal_once(Duration::from_millis(100))
            .await?
        {
            WorkerPollResult::IdentityRenewed => info!(%worker_id, "workload identity renewed"),
            WorkerPollResult::RetryScheduled => warn!(%worker_id, "identity renewal deferred"),
            WorkerPollResult::Terminated => {
                warn!(%worker_id, "invalid identity renewal terminated")
            }
            WorkerPollResult::Idle
            | WorkerPollResult::Accepted
            | WorkerPollResult::Restored
            | WorkerPollResult::Cancelled
            | WorkerPollResult::Steered
            | WorkerPollResult::ApprovalApplied
            | WorkerPollResult::ToolExecutionRequested
            | WorkerPollResult::ToolExecutionStarted
            | WorkerPollResult::ToolResultPublished
            | WorkerPollResult::ModelEventPublished
            | WorkerPollResult::ModelExecutionFinished => {}
        }
        let execution_poll = if worker.is_draining() || shutdown.is_cancelled() {
            None
        } else {
            Some(worker.poll_once(Duration::from_millis(200)).await?)
        };
        if let Some(execution_poll) = execution_poll {
            match execution_poll {
                WorkerPollResult::Idle => {}
                WorkerPollResult::Accepted => info!(%worker_id, "execution accepted"),
                WorkerPollResult::Restored => info!(%worker_id, "execution restored"),
                WorkerPollResult::Cancelled => info!(%worker_id, "execution cancelled"),
                WorkerPollResult::Steered => info!(%worker_id, "execution steered"),
                WorkerPollResult::ApprovalApplied => info!(%worker_id, "tool approval applied"),
                WorkerPollResult::ToolExecutionRequested => {
                    info!(%worker_id, "tool execution requested")
                }
                WorkerPollResult::ToolExecutionStarted => {
                    info!(%worker_id, "tool execution started")
                }
                WorkerPollResult::ToolResultPublished => info!(%worker_id, "tool result published"),
                WorkerPollResult::IdentityRenewed
                | WorkerPollResult::ModelEventPublished
                | WorkerPollResult::ModelExecutionFinished => {}
                WorkerPollResult::RetryScheduled => warn!(%worker_id, "execution deferred"),
                WorkerPollResult::Terminated => warn!(%worker_id, "invalid execution terminated"),
            }
        }
        let recovery_poll = if worker.is_draining() || shutdown.is_cancelled() {
            None
        } else {
            Some(
                worker
                    .poll_recovery_once(Duration::from_millis(100))
                    .await?,
            )
        };
        if let Some(recovery_poll) = recovery_poll {
            match recovery_poll {
                WorkerPollResult::Restored => info!(%worker_id, "execution restored"),
                WorkerPollResult::RetryScheduled => warn!(%worker_id, "recovery deferred"),
                WorkerPollResult::Terminated => warn!(%worker_id, "invalid recovery terminated"),
                WorkerPollResult::Idle
                | WorkerPollResult::Accepted
                | WorkerPollResult::IdentityRenewed
                | WorkerPollResult::Cancelled
                | WorkerPollResult::Steered
                | WorkerPollResult::ApprovalApplied
                | WorkerPollResult::ToolExecutionRequested
                | WorkerPollResult::ToolExecutionStarted
                | WorkerPollResult::ToolResultPublished
                | WorkerPollResult::ModelEventPublished
                | WorkerPollResult::ModelExecutionFinished => {}
            }
        }
        match worker
            .poll_cancellation_once(Duration::from_millis(100))
            .await?
        {
            WorkerPollResult::Cancelled => info!(%worker_id, "execution cancelled"),
            WorkerPollResult::Terminated => warn!(%worker_id, "invalid cancellation terminated"),
            WorkerPollResult::Idle
            | WorkerPollResult::Accepted
            | WorkerPollResult::IdentityRenewed
            | WorkerPollResult::Restored
            | WorkerPollResult::Steered
            | WorkerPollResult::ApprovalApplied
            | WorkerPollResult::ToolExecutionRequested
            | WorkerPollResult::ToolExecutionStarted
            | WorkerPollResult::ToolResultPublished
            | WorkerPollResult::ModelEventPublished
            | WorkerPollResult::ModelExecutionFinished
            | WorkerPollResult::RetryScheduled => {}
        }
        match worker
            .poll_steering_once(Duration::from_millis(100))
            .await?
        {
            WorkerPollResult::Steered => info!(%worker_id, "execution steered"),
            WorkerPollResult::RetryScheduled => warn!(%worker_id, "execution steering deferred"),
            WorkerPollResult::Terminated => warn!(%worker_id, "invalid steering terminated"),
            WorkerPollResult::Idle
            | WorkerPollResult::Accepted
            | WorkerPollResult::IdentityRenewed
            | WorkerPollResult::Restored
            | WorkerPollResult::Cancelled
            | WorkerPollResult::ApprovalApplied
            | WorkerPollResult::ToolExecutionRequested
            | WorkerPollResult::ToolExecutionStarted
            | WorkerPollResult::ToolResultPublished
            | WorkerPollResult::ModelEventPublished
            | WorkerPollResult::ModelExecutionFinished => {}
        }
        match worker
            .poll_approval_once(Duration::from_millis(100))
            .await?
        {
            WorkerPollResult::ApprovalApplied => info!(%worker_id, "tool approval applied"),
            WorkerPollResult::Terminated => warn!(%worker_id, "invalid approval terminated"),
            WorkerPollResult::Idle
            | WorkerPollResult::Accepted
            | WorkerPollResult::IdentityRenewed
            | WorkerPollResult::Restored
            | WorkerPollResult::Cancelled
            | WorkerPollResult::Steered
            | WorkerPollResult::ToolExecutionRequested
            | WorkerPollResult::ToolExecutionStarted
            | WorkerPollResult::ToolResultPublished
            | WorkerPollResult::ModelEventPublished
            | WorkerPollResult::ModelExecutionFinished
            | WorkerPollResult::RetryScheduled => {}
        }
        match worker.poll_model_once(Duration::from_millis(100)).await? {
            WorkerPollResult::ModelEventPublished => {}
            WorkerPollResult::ToolExecutionRequested => {
                info!(%worker_id, "tool execution requested")
            }
            WorkerPollResult::ToolExecutionStarted => {
                info!(%worker_id, "tool execution started")
            }
            WorkerPollResult::ModelExecutionFinished => {
                info!(%worker_id, "model execution finished")
            }
            WorkerPollResult::Idle
            | WorkerPollResult::Accepted
            | WorkerPollResult::IdentityRenewed
            | WorkerPollResult::Restored
            | WorkerPollResult::Cancelled
            | WorkerPollResult::Steered
            | WorkerPollResult::ApprovalApplied
            | WorkerPollResult::ToolResultPublished
            | WorkerPollResult::RetryScheduled
            | WorkerPollResult::Terminated => {}
        }
        match worker.poll_tool_once(Duration::from_millis(100)).await? {
            WorkerPollResult::ToolResultPublished => info!(%worker_id, "tool result published"),
            WorkerPollResult::Idle
            | WorkerPollResult::Accepted
            | WorkerPollResult::IdentityRenewed
            | WorkerPollResult::Restored
            | WorkerPollResult::Cancelled
            | WorkerPollResult::Steered
            | WorkerPollResult::ApprovalApplied
            | WorkerPollResult::ToolExecutionRequested
            | WorkerPollResult::ToolExecutionStarted
            | WorkerPollResult::ModelEventPublished
            | WorkerPollResult::ModelExecutionFinished
            | WorkerPollResult::RetryScheduled
            | WorkerPollResult::Terminated => {}
        }
        if worker.is_draining() {
            let active_attempts = worker.active_attempt_count();
            if active_attempts == 0 {
                worker.publish_heartbeat().await?;
                info!(%worker_id, "worker drain completed");
                break;
            }
            if drain_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                let checkpoints = worker.publish_active_checkpoints().await?;
                worker.publish_heartbeat().await?;
                warn!(
                    %worker_id,
                    active_attempts,
                    checkpoints,
                    "worker drain deadline reached; latest safe boundaries persisted"
                );
                break;
            }
        }
    }
    health.set_ready(false);
    Ok(())
}

fn parse_drain_grace(value: &str) -> Result<Duration, Box<dyn std::error::Error>> {
    let seconds = value.parse::<u64>()?;
    if !(1..=300).contains(&seconds) {
        return Err("worker drain grace must be between 1 and 300 seconds".into());
    }
    Ok(Duration::from_secs(seconds))
}

fn trusted_native_tools_enabled(value: Option<&str>) -> Result<bool, Box<dyn std::error::Error>> {
    match value {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err("AGENT_RUNTIME_TRUSTED_NATIVE_TOOLS must be true or false".into()),
    }
}

fn install_shutdown_listener(
    health: HealthState,
    admission_fence: WorkerAdmissionFence,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match shutdown_signal().await {
            Ok(()) => {
                // Remove this incarnation from Kubernetes service discovery and
                // close admission before the main loop reaches its next polling boundary.
                admission_fence.close();
                health.set_ready(false);
                shutdown.cancel();
            }
            Err(error) => warn!(%error, "failed to listen for worker shutdown signal"),
        }
    })
}

#[cfg(unix)]
async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = terminate.recv() => Ok(()),
        result = tokio::signal::ctrl_c() => result,
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}

#[cfg(test)]
mod tests {
    use super::{install_shutdown_listener, parse_drain_grace, trusted_native_tools_enabled};
    use agent_protocol::Placement;
    use agent_runtime_health::HealthState;
    use agent_runtime_worker::WorkerProcessor;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    #[test]
    fn drain_grace_is_bounded_and_reserves_time_for_process_teardown() {
        assert_eq!(parse_drain_grace("90").unwrap(), Duration::from_secs(90));
        assert!(parse_drain_grace("0").is_err());
        assert!(parse_drain_grace("301").is_err());
        assert!(parse_drain_grace("not-a-number").is_err());
    }

    #[test]
    fn trusted_native_tools_are_disabled_by_default_and_require_an_exact_opt_in() {
        assert!(!trusted_native_tools_enabled(None).unwrap());
        assert!(!trusted_native_tools_enabled(Some("false")).unwrap());
        assert!(trusted_native_tools_enabled(Some("true")).unwrap());
        assert!(trusted_native_tools_enabled(Some("yes")).is_err());
        assert!(trusted_native_tools_enabled(Some("TRUE")).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sigterm_closes_readiness_and_admission_before_main_loop_teardown() {
        let health = HealthState::default();
        health.set_ready(true);
        let processor = WorkerProcessor::new(
            Uuid::now_v7(),
            vec![Placement::Cloud],
            1,
            "0.1.0".to_string(),
        )
        .unwrap();
        let fence = processor.admission_fence();
        let shutdown = CancellationToken::new();
        let listener = install_shutdown_listener(health.clone(), fence.clone(), shutdown.clone());
        tokio::task::yield_now().await;

        let status = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        tokio::time::timeout(Duration::from_secs(2), shutdown.cancelled())
            .await
            .expect("SIGTERM must close admission promptly");

        assert!(!health.is_ready());
        assert!(!fence.is_open());
        listener.await.unwrap();
    }
}
