use agent_grpc_security::ServerMtlsMaterials;
use agent_model_gateway::{
    AnthropicMessagesAdapter, AnthropicMessagesConfig, ModelExecutionGrpcService,
    ModelPolicyRouteResolver, OpenAiCompatibleAdapter, OpenAiCompatibleConfig,
    OpenAiResponsesAdapter, OpenAiResponsesConfig, ProviderAdapter, ProviderCredential,
    ProviderPricing, ProviderProtocol, WorkloadTokenVerifier,
};
use agent_model_gateway::mcp::McpFederationClient;
use agent_model_gateway::mcp_grpc::McpFederationGrpcService;
use agent_model_gateway_protocol::v1::mcp_federation_server::McpFederationServer;
use agent_model_gateway_protocol::v1::model_execution_server::ModelExecutionServer;
use agent_runtime_health::{HealthState, serve as serve_health};
use std::net::SocketAddr;
use std::time::Duration;
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
    let mut mcp_federation: Option<McpFederationGrpcService> = None;
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
        // The same key opens MCP credentials, so federation is available only
        // where model credentials already are. A gateway that could open one but
        // not the other would be a surprising half-configured state.
        mcp_federation = Some(McpFederationGrpcService::new(
            McpFederationClient::from_pkcs8_pem(
                &private_key_pem,
                response_timeout,
                // Default deny. A deployment that needs a loopback MCP server --
                // local development, mostly -- says so; production says nothing
                // and gets the safe answer.
                environment("AGENT_RUNTIME_MCP_ALLOW_LOOPBACK", "false") == "true",
            )?,
            WorkloadTokenVerifier::from_base64(&required_environment(
                "AGENT_RUNTIME_WORKLOAD_IDENTITY_PUBLIC_KEY",
            )?)?,
        ));
    }
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
    router
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

fn parse_environment<T>(name: &str, default: T) -> Result<T, Box<dyn std::error::Error>>
where
    T: std::str::FromStr + ToString,
    T::Err: std::error::Error + 'static,
{
    Ok(environment(name, &default.to_string()).parse::<T>()?)
}
