use agent_checkpoint_gateway::{
    CheckpointStorageGrpcService, S3CheckpointStoreConfig, build_configured_checkpoint_store,
    checkpoint_storage_server,
};
use agent_grpc_security::ServerMtlsMaterials;
use agent_runtime_health::{HealthState, serve as serve_health};
use agent_workload_identity::WorkloadTokenVerifier;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().json().init();
    let health = HealthState::default();
    let health_listener =
        tokio::net::TcpListener::bind(environment("AGENT_RUNTIME_HEALTH_BIND", "0.0.0.0:8080"))
            .await?;
    tokio::spawn(serve_health(health_listener, health.clone()));
    let bind_address = environment("AGENT_RUNTIME_CHECKPOINT_GATEWAY_BIND", "127.0.0.1:50052")
        .parse::<SocketAddr>()?;
    let local_root = optional_environment("AGENT_RUNTIME_CHECKPOINT_LOCAL_DIR").map(PathBuf::from);
    let s3_config = if local_root.is_none() {
        Some(S3CheckpointStoreConfig {
            endpoint: required_environment("AGENT_RUNTIME_CHECKPOINT_S3_ENDPOINT")?,
            bucket: required_environment("AGENT_RUNTIME_CHECKPOINT_S3_BUCKET")?,
            region: environment("AGENT_RUNTIME_CHECKPOINT_S3_REGION", "us-east-1"),
            access_key_id: required_environment("AGENT_RUNTIME_CHECKPOINT_S3_ACCESS_KEY_ID")?,
            secret_access_key: required_environment(
                "AGENT_RUNTIME_CHECKPOINT_S3_SECRET_ACCESS_KEY",
            )?,
            allow_http: parse_environment("AGENT_RUNTIME_CHECKPOINT_S3_ALLOW_HTTP", false)?,
        })
    } else {
        None
    };
    let store = build_configured_checkpoint_store(local_root.as_deref(), s3_config)?;
    let verifier = WorkloadTokenVerifier::from_base64(&required_environment(
        "AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY",
    )?)?;
    let tls = ServerMtlsMaterials::from_files(
        required_environment("AGENT_RUNTIME_GRPC_SERVER_CERT")?,
        required_environment("AGENT_RUNTIME_GRPC_SERVER_KEY")?,
        required_environment("AGENT_RUNTIME_GRPC_CLIENT_CA_CERT")?,
    )?;
    let service = CheckpointStorageGrpcService::new(store, verifier);
    let listener = tokio::net::TcpListener::bind(bind_address).await?;
    health.set_ready(true);
    info!(component = "checkpoint-gateway", %bind_address, "checkpoint gateway listening");
    Server::builder()
        .tls_config(tls.into_tonic())?
        .add_service(checkpoint_storage_server(service))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}

fn environment(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn required_environment(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    let value = std::env::var(name)?;
    if value.trim().is_empty() {
        return Err(format!("{name} must not be blank").into());
    }
    Ok(value)
}

fn optional_environment(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn parse_environment<T>(name: &str, default: T) -> Result<T, Box<dyn std::error::Error>>
where
    T: std::str::FromStr + ToString,
    T::Err: std::error::Error + 'static,
{
    Ok(environment(name, &default.to_string()).parse::<T>()?)
}
