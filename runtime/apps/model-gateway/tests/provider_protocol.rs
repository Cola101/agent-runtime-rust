use agent_model_gateway::ProviderProtocol;
use std::str::FromStr;

#[test]
fn supported_provider_protocols_have_stable_configuration_names() {
    assert_eq!(
        ProviderProtocol::from_str("openai_compatible").unwrap(),
        ProviderProtocol::OpenAiCompatible
    );
    assert_eq!(
        ProviderProtocol::from_str("openai_responses").unwrap(),
        ProviderProtocol::OpenAiResponses
    );
    assert_eq!(
        ProviderProtocol::from_str("anthropic_messages").unwrap(),
        ProviderProtocol::AnthropicMessages
    );
    assert!(ProviderProtocol::from_str("chat").is_err());
}
