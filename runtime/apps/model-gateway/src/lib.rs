use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use agent_protocol::{ContentPart, ModelRequest, ModelStreamEvent};

mod anthropic_messages;
mod failover;
mod grpc;
mod invocation;
pub mod mcp;
pub mod mcp_grpc;
pub mod mcp_oauth;
pub mod mcp_oauth_grpc;
mod openai_compatible;
mod openai_responses;
mod provider_registry;

pub use agent_workload_identity::{WorkloadIdentityClaims, WorkloadTokenVerifier};
pub use failover::{
    FailoverSelection, ProviderRoute, execute_with_frozen_failover, execute_with_safe_failover,
};
pub use grpc::ModelExecutionGrpcService;
pub use invocation::{ModelInvocationDecodeError, decode_model_invocation};
pub use openai_compatible::{
    OpenAiCompatibleAdapter, OpenAiCompatibleConfig, ProviderCredential, ProviderExecutionError,
    ProviderPricing,
};
pub use openai_responses::{OpenAiResponsesAdapter, OpenAiResponsesConfig};
pub use provider_registry::ModelPolicyRouteResolver;

/// The aliases are not decoration. `rename_all = "snake_case"` turns
/// `OpenAiCompatible` into `open_ai_compatible`, while `FromStr` below -- and
/// therefore `provider_registry`, `edge-node` and the gateway's own CLI -- has
/// always taken `openai_compatible`. One protocol with two spellings, and the
/// one a person writes was the one the config parser rejected: a desktop
/// client wrote a routing file from the spelling every other config path uses
/// and `runtime-host` exited before it listened.
///
/// Accepting both is additive: the serialized form is unchanged, so nothing
/// already written to disk or folded into a digest moves.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    #[serde(alias = "openai_compatible")]
    OpenAiCompatible,
    #[serde(alias = "openai_responses")]
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
        provider_id: &str,
        request: &ModelRequest,
        credential: &ProviderCredential,
        cancellation: CancellationToken,
        events: mpsc::Sender<ModelStreamEvent>,
    ) -> Result<(), ProviderExecutionError> {
        if provider_id.trim().is_empty() {
            return Err(ProviderExecutionError::InvalidConfiguration(
                "provider route id must not be blank".into(),
            ));
        }
        let (request, omissions) =
            prepare_request_for_provider(request, provider_id, self.protocol_name(), self.model())?;
        for omission in omissions {
            openai_compatible::emit(&events, omission).await?;
        }
        match self {
            Self::OpenAiCompatible(adapter) => {
                adapter
                    .execute(&request, credential, cancellation, events)
                    .await
            }
            Self::OpenAiResponses(adapter) => {
                adapter
                    .execute_for_provider(provider_id, &request, credential, cancellation, events)
                    .await
            }
            Self::AnthropicMessages(adapter) => {
                adapter
                    .execute_for_provider(provider_id, &request, credential, cancellation, events)
                    .await
            }
        }
    }

    fn protocol_name(&self) -> &'static str {
        match self {
            Self::OpenAiCompatible(_) => "openai_compatible",
            Self::OpenAiResponses(_) => "openai_responses",
            Self::AnthropicMessages(_) => "anthropic_messages",
        }
    }

    fn model(&self) -> &str {
        match self {
            Self::OpenAiCompatible(adapter) => adapter.model(),
            Self::OpenAiResponses(adapter) => adapter.model(),
            Self::AnthropicMessages(adapter) => adapter.model(),
        }
    }
}

fn prepare_request_for_provider(
    request: &ModelRequest,
    target_provider_id: &str,
    target_protocol: &str,
    target_model: &str,
) -> Result<(ModelRequest, Vec<ModelStreamEvent>), ProviderExecutionError> {
    let mut request = request.clone();
    let mut omissions = Vec::new();
    for message in &mut request.messages {
        for part in &mut message.content {
            let ContentPart::Reasoning { private_state, .. } = part else {
                continue;
            };
            let Some(state) = private_state else {
                continue;
            };
            if !state.is_well_formed() {
                return Err(ProviderExecutionError::InvalidConfiguration(
                    "provider-private model state is malformed".into(),
                ));
            }
            if state.provider_id == target_provider_id
                && state.protocol == target_protocol
                && state.model == target_model
            {
                continue;
            }
            omissions.push(ModelStreamEvent::PrivateStateOmitted {
                origin_provider_id: state.provider_id.clone(),
                target_provider_id: target_provider_id.to_owned(),
                format: state.format.clone(),
            });
            *private_state = None;
        }
    }
    Ok((request, omissions))
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataClass {
    Public,
    Internal,
    Confidential,
    Restricted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
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
