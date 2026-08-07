use std::collections::BTreeSet;
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use agent_protocol::{ModelRequest, ModelStreamEvent};

pub mod mcp;
pub mod mcp_grpc;
mod anthropic_messages;
mod failover;
mod grpc;
mod invocation;
mod openai_compatible;
mod openai_responses;
mod provider_registry;

pub use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
pub use failover::{FailoverSelection, ProviderRoute, execute_with_safe_failover};
pub use grpc::ModelExecutionGrpcService;
pub use invocation::{ModelInvocationDecodeError, decode_model_invocation};
pub use openai_compatible::{
    OpenAiCompatibleAdapter, OpenAiCompatibleConfig, ProviderCredential, ProviderExecutionError,
    ProviderPricing,
};
pub use openai_responses::{OpenAiResponsesAdapter, OpenAiResponsesConfig};
pub use provider_registry::ModelPolicyRouteResolver;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderProtocol {
    OpenAiCompatible,
    OpenAiResponses,
    AnthropicMessages,
}

impl FromStr for ProviderProtocol {
    type Err = ProviderExecutionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "openai_compatible" => Ok(Self::OpenAiCompatible),
            "openai_responses" => Ok(Self::OpenAiResponses),
            "anthropic_messages" => Ok(Self::AnthropicMessages),
            _ => Err(ProviderExecutionError::InvalidConfiguration(format!(
                "unsupported provider protocol {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ProviderAdapter {
    OpenAiCompatible(OpenAiCompatibleAdapter),
    OpenAiResponses(OpenAiResponsesAdapter),
    AnthropicMessages(AnthropicMessagesAdapter),
}

impl ProviderAdapter {
    pub async fn execute(
        &self,
        request: &ModelRequest,
        credential: &ProviderCredential,
        cancellation: CancellationToken,
        events: mpsc::Sender<ModelStreamEvent>,
    ) -> Result<(), ProviderExecutionError> {
        match self {
            Self::OpenAiCompatible(adapter) => {
                adapter
                    .execute(request, credential, cancellation, events)
                    .await
            }
            Self::OpenAiResponses(adapter) => {
                adapter
                    .execute(request, credential, cancellation, events)
                    .await
            }
            Self::AnthropicMessages(adapter) => {
                adapter
                    .execute(request, credential, cancellation, events)
                    .await
            }
        }
    }
}

impl From<OpenAiCompatibleAdapter> for ProviderAdapter {
    fn from(value: OpenAiCompatibleAdapter) -> Self {
        Self::OpenAiCompatible(value)
    }
}

impl From<OpenAiResponsesAdapter> for ProviderAdapter {
    fn from(value: OpenAiResponsesAdapter) -> Self {
        Self::OpenAiResponses(value)
    }
}

impl From<AnthropicMessagesAdapter> for ProviderAdapter {
    fn from(value: AnthropicMessagesAdapter) -> Self {
        Self::AnthropicMessages(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DataClass {
    Public,
    Internal,
    Confidential,
    Restricted,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Capability {
    Text,
    Vision,
    Audio,
    ToolUse,
    StructuredOutput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCandidate {
    pub id: String,
    pub region: String,
    pub accepted_data_classes: BTreeSet<DataClass>,
    pub capabilities: BTreeSet<Capability>,
    pub healthy: bool,
    pub latency_ms: u64,
    pub cost_per_million_tokens_micros: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutingConstraints {
    pub allowed_regions: BTreeSet<String>,
    pub data_class: DataClass,
    pub required_capabilities: BTreeSet<Capability>,
    pub max_cost_per_million_tokens_micros: u64,
}

#[must_use]
pub fn rank_candidates<'a>(
    candidates: &'a [ModelCandidate],
    constraints: &RoutingConstraints,
) -> Vec<&'a ModelCandidate> {
    let mut eligible = candidates
        .iter()
        .filter(|candidate| candidate.healthy)
        .filter(|candidate| constraints.allowed_regions.contains(&candidate.region))
        .filter(|candidate| {
            candidate
                .accepted_data_classes
                .contains(&constraints.data_class)
        })
        .filter(|candidate| {
            constraints
                .required_capabilities
                .is_subset(&candidate.capabilities)
        })
        .filter(|candidate| {
            candidate.cost_per_million_tokens_micros
                <= constraints.max_cost_per_million_tokens_micros
        })
        .collect::<Vec<_>>();
    eligible.sort_by_key(|candidate| {
        (
            candidate.latency_ms,
            candidate.cost_per_million_tokens_micros,
            candidate.id.as_str(),
        )
    });
    eligible
}
pub use anthropic_messages::{AnthropicMessagesAdapter, AnthropicMessagesConfig};
