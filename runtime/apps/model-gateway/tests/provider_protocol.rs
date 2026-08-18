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

/// One protocol, two spellings, and until this test both were live: `FromStr`
/// took `openai_compatible` while serde required `open_ai_compatible`. A
/// routing file written in the spelling every other config path uses made
/// `runtime-host` exit before it listened, with a message about an unknown
/// variant rather than about a misspelling.
#[test]
fn both_spellings_of_a_protocol_deserialize_to_the_same_variant() {
    for (written, expected) in [
        ("openai_compatible", ProviderProtocol::OpenAiCompatible),
        ("open_ai_compatible", ProviderProtocol::OpenAiCompatible),
        ("openai_responses", ProviderProtocol::OpenAiResponses),
        ("open_ai_responses", ProviderProtocol::OpenAiResponses),
        ("anthropic_messages", ProviderProtocol::AnthropicMessages),
    ] {
        let parsed: ProviderProtocol = serde_json::from_str(&format!("\"{written}\""))
            .unwrap_or_else(|error| {
                panic!("{written} is a spelling of a protocol this system uses: {error}")
            });
        assert_eq!(parsed, expected, "{written}");
        // Whatever a config is written in, only one spelling is ever produced.
        // Changing that would move every digest a serialized protocol feeds.
        assert_eq!(
            serde_json::to_string(&parsed).expect("a protocol serializes"),
            format!(
                "\"{}\"",
                match expected {
                    ProviderProtocol::OpenAiCompatible => "open_ai_compatible",
                    ProviderProtocol::OpenAiResponses => "open_ai_responses",
                    ProviderProtocol::AnthropicMessages => "anthropic_messages",
                }
            ),
        );
    }
    assert!(serde_json::from_str::<ProviderProtocol>("\"chat\"").is_err());
}
