use agent_grpc_security::ServerMtlsMaterials;
use agent_model_gateway::mcp::McpFederationClient;
use agent_model_gateway::mcp_grpc::McpFederationGrpcService;
use agent_model_gateway::mcp_oauth::McpOAuthCoordinator;
use agent_model_gateway::mcp_oauth_grpc::McpOAuthAdminGrpcService;
use agent_model_gateway::{
    AnthropicMessagesAdapter, AnthropicMessagesConfig, ModelExecutionGrpcService,
    ModelPolicyRouteResolver, OpenAiCompatibleAdapter, OpenAiCompatibleConfig,
    OpenAiResponsesAdapter, OpenAiResponsesConfig, ProviderAdapter, ProviderCredential,
    ProviderPricing, ProviderProtocol, WorkloadTokenVerifier,
};
use agent_model_gateway_protocol::v1::mcp_federation_server::McpFederationServer;
use agent_model_gateway_protocol::v1::mcp_oauth_admin_server::McpOauthAdminServer;
use agent_model_gateway_protocol::v1::model_execution_server::ModelExecutionServer;
use agent_runtime_health::{HealthState, serve as serve_health};
use base64::Engine as _;
use std::io::Read as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tracing::info;
use zeroize::Zeroizing;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().json().init();
    let health = HealthState::default();
    let health_listener =
        tokio::net::TcpListener::bind(environment("AGENT_RUNTIME_HEALTH_BIND", "0.0.0.0:8080"))
            .await?;
    tokio::spawn(serve_health(health_listener, health.clone()));
    let bind_address =
        environment("AGENT_RUNTIME_MODEL_GATEWAY_BIND", "127.0.0.1:50051").parse::<SocketAddr>()?;
    let protocol = environment("AGENT_RUNTIME_PROVIDER_PROTOCOL", "openai_compatible")
        .parse::<ProviderProtocol>()?;
    let endpoint = std::env::var("AGENT_RUNTIME_PROVIDER_ENDPOINT")?;
    let model = std::env::var("AGENT_RUNTIME_PROVIDER_MODEL")?;
    let pricing = ProviderPricing {
        input_million_tokens_micros: parse_environment(
            "AGENT_RUNTIME_PROVIDER_INPUT_MILLION_TOKENS_MICROS",
            0,
        )?,
        output_million_tokens_micros: parse_environment(
            "AGENT_RUNTIME_PROVIDER_OUTPUT_MILLION_TOKENS_MICROS",
            0,
        )?,
        // Unset means unset, not zero: an absent rate bills a cache hit at the
        // full input rate above, and a zero declared here means the operator is
        // saying cache hits are free -- which is true of a self-hosted server
        // and false of every paid API.
        cached_input_million_tokens_micros: optional_parse_environment(
            "AGENT_RUNTIME_PROVIDER_CACHED_INPUT_MILLION_TOKENS_MICROS",
        )?,
    };
    let response_timeout = Duration::from_secs(parse_environment(
        "AGENT_RUNTIME_PROVIDER_RESPONSE_TIMEOUT_SECONDS",
        30,
    )?);
    let stream_idle_timeout = Duration::from_secs(parse_environment(
        "AGENT_RUNTIME_PROVIDER_STREAM_IDLE_TIMEOUT_SECONDS",
        60,
    )?);
    let adapter = match protocol {
        ProviderProtocol::OpenAiCompatible => {
            ProviderAdapter::from(OpenAiCompatibleAdapter::new(OpenAiCompatibleConfig {
                endpoint,
                model,
                pricing,
                response_timeout,
                stream_idle_timeout,
                max_output_tokens: None,
                supports_reasoning_effort: parse_environment(
                    "AGENT_RUNTIME_PROVIDER_SUPPORTS_REASONING_EFFORT",
                    false,
                )?,
            })?)
        }
        ProviderProtocol::OpenAiResponses => {
            ProviderAdapter::from(OpenAiResponsesAdapter::new(OpenAiResponsesConfig {
                endpoint,
                model,
                pricing,
                response_timeout,
                stream_idle_timeout,
            })?)
        }
        ProviderProtocol::AnthropicMessages => {
            ProviderAdapter::from(AnthropicMessagesAdapter::new(AnthropicMessagesConfig {
                endpoint,
                model,
                anthropic_version: environment(
                    "AGENT_RUNTIME_PROVIDER_ANTHROPIC_VERSION",
                    "2023-06-01",
                ),
                pricing,
                response_timeout,
                stream_idle_timeout,
            })?)
        }
    };
    let credential = ProviderCredential::bearer(std::env::var("AGENT_RUNTIME_PROVIDER_API_KEY")?)?;
    let verifier = WorkloadTokenVerifier::from_base64(&required_environment(
        "AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY",
    )?)?;
    let tls = ServerMtlsMaterials::from_files(
        required_environment("AGENT_RUNTIME_GRPC_SERVER_CERT")?,
        required_environment("AGENT_RUNTIME_GRPC_SERVER_KEY")?,
        required_environment("AGENT_RUNTIME_GRPC_CLIENT_CA_CERT")?,
    )?;
    let mut service = ModelExecutionGrpcService::new(adapter, credential, verifier);
    let loopback_permitted = environment("AGENT_RUNTIME_MCP_ALLOW_LOOPBACK", "false") == "true";
    let mut mcp_client = None;
    if let Ok(private_key_path) =
        std::env::var("AGENT_RUNTIME_MODEL_GATEWAY_CREDENTIAL_PRIVATE_KEY_PATH")
    {
        if private_key_path.trim().is_empty() {
            return Err("model gateway credential private key path must not be blank".into());
        }
        let private_key_pem = std::fs::read_to_string(private_key_path)?;
        service = service.with_route_resolver(ModelPolicyRouteResolver::from_pkcs8_pem(
            &private_key_pem,
            response_timeout,
            stream_idle_timeout,
        )?);
        mcp_client = Some(McpFederationClient::from_pkcs8_pem(
            &private_key_pem,
            response_timeout,
            loopback_permitted,
        )?);
    }
    // Shared, not duplicated: the federation client resolves tokens through the
    // same coordinator the admin surface mutates, so an operator revoking a
    // credential is immediately visible to in-flight federation calls.
    let mut oauth_coordinator = None;
    if let Some(coordinator) =
        oauth_coordinator_from_environment(response_timeout, loopback_permitted)?
    {
        let coordinator = Arc::new(coordinator);
        let client = match mcp_client.take() {
            Some(client) => client,
            None => McpFederationClient::for_open_servers(response_timeout, loopback_permitted)?,
        };
        mcp_client = Some(client.with_oauth_coordinator(Arc::clone(&coordinator)));
        oauth_coordinator = Some(coordinator);
    }
    let mcp_federation = mcp_client
        .map(|client| {
            Ok::<_, Box<dyn std::error::Error>>(McpFederationGrpcService::new(
                client,
                WorkloadTokenVerifier::from_base64(&required_environment(
                    "AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY",
                )?)?,
            ))
        })
        .transpose()?;
    let oauth_admin = oauth_coordinator
        .map(|coordinator| {
            Ok::<_, Box<dyn std::error::Error>>(McpOAuthAdminGrpcService::new(
                coordinator,
                WorkloadTokenVerifier::from_base64(&required_environment(
                    "AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY",
                )?)?,
            ))
        })
        .transpose()?;
    let listener = tokio::net::TcpListener::bind(bind_address).await?;
    health.set_ready(true);
    info!(
        component = "model-gateway",
        %bind_address,
        "model gateway listening"
    );
    let mut server = Server::builder().tls_config(tls.into_tonic())?;
    let router = server.add_service(ModelExecutionServer::new(service));
    let router = match mcp_federation {
        Some(federation) => router.add_service(McpFederationServer::new(federation)),
        None => router,
    };
    let router = match oauth_admin {
        Some(admin) => router.add_service(McpOauthAdminServer::new(admin)),
        None => router,
    };
    router
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await?;
    Ok(())
}

fn oauth_coordinator_from_environment(
    request_timeout: Duration,
    loopback_permitted: bool,
) -> Result<Option<McpOAuthCoordinator>, Box<dyn std::error::Error>> {
    let (state_root, master_key_path) = match (
        std::env::var("AGENT_RUNTIME_MCP_OAUTH_STATE_ROOT").ok(),
        std::env::var("AGENT_RUNTIME_MCP_OAUTH_MASTER_KEY_FILE").ok(),
    ) {
        (None, None) => return Ok(None),
        (Some(state_root), Some(master_key_path)) => (state_root, master_key_path),
        _ => {
            return Err(
                "MCP OAuth state root and master key file must be configured together".into(),
            );
        }
    };
    if state_root.trim().is_empty() || master_key_path.trim().is_empty() {
        return Err("MCP OAuth paths must not be blank".into());
    }
    #[cfg(unix)]
    let key_file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&master_key_path)?
    };
    #[cfg(not(unix))]
    let key_file = std::fs::File::open(&master_key_path)?;
    let metadata = key_file.metadata()?;
    if !metadata.is_file() || metadata.len() > 1_024 {
        return Err("MCP OAuth master key must be a small regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.permissions().mode() & 0o077 != 0
            // SAFETY: geteuid has no preconditions and does not mutate process state.
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(
                "MCP OAuth master key file must be owned by this user and mode 0600 or stricter"
                    .into(),
            );
        }
    }
    let mut encoded_key = Zeroizing::new(String::new());
    key_file.take(1_025).read_to_string(&mut encoded_key)?;
    if encoded_key.len() > 1_024 {
        return Err("MCP OAuth master key file is too large".into());
    }
    let decoded_key =
        Zeroizing::new(base64::engine::general_purpose::STANDARD.decode(encoded_key.trim())?);
    let master_key: [u8; 32] = decoded_key
        .as_slice()
        .try_into()
        .map_err(|_| "MCP OAuth master key must decode to exactly 32 bytes")?;
    Ok(Some(McpOAuthCoordinator::new(
        state_root,
        master_key,
        request_timeout,
        loopback_permitted,
    )?))
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

fn parse_environment<T>(name: &str, default: T) -> Result<T, Box<dyn std::error::Error>>
where
    T: std::str::FromStr + ToString,
    T::Err: std::error::Error + 'static,
{
    Ok(environment(name, &default.to_string()).parse::<T>()?)
}

/// Like `parse_environment`, for a setting whose absence is a fact rather than
/// a number: unset stays `None` instead of collapsing onto a default that would
/// claim something the operator never said.
fn optional_parse_environment<T>(name: &str) -> Result<Option<T>, Box<dyn std::error::Error>>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + 'static,
{
    match std::env::var(name) {
        Ok(value) => Ok(Some(value.parse::<T>()?)),
        Err(_) => Ok(None),
    }
}
