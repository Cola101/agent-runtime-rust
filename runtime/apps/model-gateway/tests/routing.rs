use agent_model_gateway::{
    Capability, DataClass, ModelCandidate, RoutingConstraints, rank_candidates,
};
use std::collections::BTreeSet;

fn candidate(
    id: &str,
    region: &str,
    capabilities: &[Capability],
    latency_ms: u64,
    cost_per_million: u64,
) -> ModelCandidate {
    ModelCandidate {
        id: id.into(),
        region: region.into(),
        accepted_data_classes: BTreeSet::from([DataClass::Confidential]),
        capabilities: capabilities.iter().copied().collect(),
        healthy: true,
        latency_ms,
        cost_per_million_tokens_micros: cost_per_million,
    }
}

#[test]
fn policy_filters_before_latency_and_price_ranking() {
    let candidates = vec![
        candidate("cheap-us", "us-east", &[Capability::Text], 10, 1),
        candidate("eu-text", "eu-west", &[Capability::Text], 20, 2),
        candidate(
            "eu-vision-slow",
            "eu-west",
            &[Capability::Text, Capability::Vision],
            40,
            3,
        ),
        candidate(
            "eu-vision-fast",
            "eu-west",
            &[Capability::Text, Capability::Vision],
            15,
            6,
        ),
    ];
    let constraints = RoutingConstraints {
        allowed_regions: BTreeSet::from(["eu-west".into()]),
        data_class: DataClass::Confidential,
        required_capabilities: BTreeSet::from([Capability::Vision]),
        max_cost_per_million_tokens_micros: 10,
    };

    let ranked = rank_candidates(&candidates, &constraints);

    assert_eq!(
        ranked
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["eu-vision-fast", "eu-vision-slow"]
    );
}

#[test]
fn unhealthy_or_over_budget_candidates_are_not_routable() {
    let mut unhealthy = candidate("unhealthy", "eu-west", &[Capability::Text], 1, 1);
    unhealthy.healthy = false;
    let expensive = candidate("expensive", "eu-west", &[Capability::Text], 1, 500);
    let constraints = RoutingConstraints {
        allowed_regions: BTreeSet::from(["eu-west".into()]),
        data_class: DataClass::Confidential,
        required_capabilities: BTreeSet::from([Capability::Text]),
        max_cost_per_million_tokens_micros: 100,
    };

    assert!(rank_candidates(&[unhealthy, expensive], &constraints).is_empty());
}
