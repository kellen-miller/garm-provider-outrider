use super::config::{candidate, charge, profile};
use std::collections::BTreeMap;
use std::io;

use outrider::{
    config::RunnerProfile,
    policy::{
        MissingDataBehavior, Policy, PolicyDefinition, PolicyError, WeightedPolicy, rank,
        rank_configured,
    },
};

use rust_decimal::Decimal;

#[test]
fn all_four_policies() {
    let profile = profile().unwrap();
    let candidates = vec![
        candidate("b", Decimal::ONE),
        candidate("a", Decimal::new(50, 2)),
    ];
    let economy = Policy::Economy {
        max_price_age_seconds: 600,
    };

    assert_eq!(
        rank(&economy, &candidates, &profile).unwrap()[0].identity,
        "a"
    );
    let mut preferred = profile.clone();
    preferred.purchasing_models = ["on_demand".to_owned(), "spot".to_owned()].into();
    preferred.purchasing_tiers = vec![vec!["on_demand".to_owned()], vec!["spot".to_owned()]];
    let mut cheapest_spot = candidate("spot", Decimal::new(1, 2));
    cheapest_spot.purchasing = "spot".to_owned();
    let on_demand = candidate("on-demand", Decimal::ONE);
    assert_eq!(
        rank(&economy, &[cheapest_spot, on_demand], &preferred).unwrap()[0].identity,
        "on-demand"
    );
    let continuity = Policy::Continuity {
        max_observation_age_seconds: 600,
        min_samples: 10,
    };

    assert_eq!(
        rank(&continuity, &candidates, &profile).unwrap()[0].identity,
        "a"
    );
    let mut low_interrupt_high_failure = candidate("low-interrupt", Decimal::ONE);
    low_interrupt_high_failure.interruption_rate_per_hour = Some(Decimal::new(1, 6));
    low_interrupt_high_failure.allocation_failure_rate = Some(Decimal::ONE);
    let mut high_interrupt_low_failure = candidate("high-interrupt", Decimal::ONE);
    high_interrupt_low_failure.interruption_rate_per_hour = Some(Decimal::new(11, 7));
    high_interrupt_low_failure.allocation_failure_rate = Some(Decimal::ZERO);
    assert_eq!(
        rank(
            &continuity,
            &[high_interrupt_low_failure, low_interrupt_high_failure],
            &profile
        )
        .unwrap()[0]
            .identity,
        "low-interrupt"
    );
    let weighted = Policy::Weighted(WeightedPolicy {
        cost_weight: Decimal::ONE,
        failure_weight: Decimal::ZERO,
        interruption_weight: Decimal::ZERO,
        latency_weight: Decimal::ZERO,
        cost_scale: Decimal::ONE,
        interruption_scale: Decimal::ONE,
        latency_scale: Decimal::ONE,
        min_samples: 10,
        max_observation_age_seconds: 600,
        max_price_age_seconds: 600,
    });
    let weighted_rank = rank(&weighted, &candidates, &profile).unwrap();
    assert_eq!(weighted_rank[0].identity, "a");
    assert_eq!(weighted_rank[0].score_terms.len(), 1);
    assert_eq!(weighted_rank[0].score_terms[0].dimension, "cost");
    assert_eq!(weighted_rank[0].score_terms[0].weight, Decimal::ONE);
    assert_eq!(weighted_rank[0].score_terms[0].scale, Decimal::ONE);
    let ordered = Policy::Ordered {
        identities: vec!["b".to_owned(), "a".to_owned()],
    };

    assert_eq!(
        rank(&ordered, &candidates, &profile).unwrap()[0].identity,
        "b"
    );
    let mut price_not_required = candidate("ordered-without-price", Decimal::ONE);
    price_not_required.price = None;
    let mut no_cost_ceiling = profile.clone();
    no_cost_ceiling.maximum_total_cost = None;
    no_cost_ceiling.administrator_limits.max_total_cost = None;
    assert_eq!(
        rank(
            &Policy::Ordered {
                identities: vec!["ordered-without-price".to_owned()]
            },
            &[price_not_required],
            &no_cost_ceiling
        )
        .unwrap()[0]
            .identity,
        "ordered-without-price"
    );
    assert_eq!(
        candidate("a", Decimal::ONE)
            .meets(&RunnerProfile {
                architecture: "arm64".to_owned(),
                ..profile
            })
            .unwrap_err(),
        "architecture mismatch"
    );
}

#[test]
fn cost_horizon_ranking_consistency() {
    let mut profile = profile().unwrap();
    profile.maximum_total_cost = None;
    profile.administrator_limits.max_total_cost = None;
    let mut committed = candidate("committed", Decimal::ONE);
    let committed_quote = committed.price.as_mut().unwrap();
    committed_quote.compute.minimum_commitment_seconds = 86_400;
    committed_quote.disk = charge(Decimal::ZERO);
    let mut hourly = candidate("hourly", Decimal::from(2));
    hourly.price.as_mut().unwrap().disk = charge(Decimal::ZERO);
    let candidates = [committed.clone(), hourly.clone()];
    let economy = Policy::Economy {
        max_price_age_seconds: 600,
    };

    let ranked_economy = rank(&economy, &candidates, &profile).unwrap();
    assert_eq!(ranked_economy[0].identity, "committed");
    assert_eq!(ranked_economy[0].score, Some(Decimal::from(24)));
    assert_eq!(ranked_economy[1].identity, "hourly");
    assert_eq!(ranked_economy[1].score, Some(Decimal::from(48)));
    assert!(ranked_economy[0].reason.contains("over 86400s"));

    let weighted = Policy::Weighted(WeightedPolicy {
        cost_weight: Decimal::ONE,
        failure_weight: Decimal::ZERO,
        interruption_weight: Decimal::ZERO,
        latency_weight: Decimal::ZERO,
        cost_scale: Decimal::ONE,
        interruption_scale: Decimal::ONE,
        latency_scale: Decimal::ONE,
        min_samples: 10,
        max_observation_age_seconds: 600,
        max_price_age_seconds: 600,
    });
    let ranked_weighted = rank(&weighted, &candidates, &profile).unwrap();
    assert_eq!(ranked_weighted[0].identity, "committed");
    assert_eq!(ranked_weighted[0].score, Some(Decimal::from(24)));
    assert_eq!(ranked_weighted[1].score, Some(Decimal::from(48)));
    assert!(ranked_weighted[0].reason.contains("over 86400s"));

    profile.maximum_total_cost = Some(Decimal::from(30));
    let capped = rank(&economy, &candidates, &profile).unwrap();
    assert!(capped[0].eligible);
    assert_eq!(capped[0].identity, "committed");
    assert!(!capped[1].eligible);
    assert_eq!(capped[1].reason, "above maximum total cost");

    let mut definitions = BTreeMap::new();
    definitions.insert(
        "continuity".to_owned(),
        PolicyDefinition {
            strategy: Policy::Continuity {
                max_observation_age_seconds: 600,
                min_samples: 10,
            },
            missing_data: MissingDataBehavior::Fallback {
                policy: "economy".to_owned(),
            },
        },
    );
    definitions.insert(
        "economy".to_owned(),
        PolicyDefinition {
            strategy: economy,
            missing_data: MissingDataBehavior::Reject,
        },
    );
    let mut no_observations = committed;
    no_observations.observation_age_seconds = None;
    no_observations.observation_ages = None;
    no_observations.observation_window = None;
    no_observations.observation_samples = 0;
    let fallback =
        rank_configured("continuity", &definitions, &[no_observations], &profile).unwrap();
    assert_eq!(fallback.applied_policy, "economy");
    assert_eq!(fallback.fallback_from.as_deref(), Some("continuity"));
    assert_eq!(fallback.candidates[0].score, Some(Decimal::from(24)));
    assert!(fallback.candidates[0].reason.contains("over 86400s"));
}

#[test]
fn missing_stale_data() {
    let mut profile = profile().unwrap();
    profile.maximum_total_cost = None;
    profile.administrator_limits.max_total_cost = None;
    let mut missing = candidate("a", Decimal::ONE);
    missing.price = None;
    let economy = Policy::Economy {
        max_price_age_seconds: 60,
    };

    assert_eq!(
        rank(&economy, &[missing.clone()], &profile).unwrap_err(),
        PolicyError::MissingData("price")
    );
    let mut stale_price = candidate("a", Decimal::ONE);
    stale_price.price.as_mut().unwrap().observed_age_seconds = Some(61);
    assert_eq!(
        rank(&economy, &[stale_price], &profile).unwrap_err(),
        PolicyError::MissingData("fresh price")
    );
    let mut ceiling_profile = profile.clone();
    ceiling_profile.maximum_total_cost = Some(Decimal::ONE);
    let mut stale_ceiling_quote = candidate("a", Decimal::ONE);
    stale_ceiling_quote
        .price
        .as_mut()
        .unwrap()
        .observed_age_seconds = Some(3601);
    assert_eq!(
        rank(
            &Policy::Ordered {
                identities: vec!["a".to_owned()]
            },
            &[stale_ceiling_quote],
            &ceiling_profile
        )
        .unwrap_err(),
        PolicyError::MissingData("fresh price for cost ceiling")
    );
    let mut stale_observation = candidate("a", Decimal::ONE);
    stale_observation.observation_age_seconds = Some(1000);
    stale_observation.observation_ages = Some(outrider::policy::ObservationAges {
        create_seconds: Some(1000),
        latency_seconds: Some(1000),
        interruption_seconds: Some(1000),
    });
    assert_eq!(
        rank(
            &Policy::Continuity {
                max_observation_age_seconds: 600,
                min_samples: 10
            },
            &[stale_observation],
            &profile
        )
        .unwrap_err(),
        PolicyError::MissingData("adequate recent observations")
    );
    let continuity = Policy::Continuity {
        max_observation_age_seconds: 600,
        min_samples: 10,
    };

    assert_invalid_continuity_observations(&profile, &continuity).unwrap();
}

fn assert_invalid_continuity_observations(
    profile: &RunnerProfile,
    continuity: &Policy,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut negative_interruption = candidate("negative-interruption", Decimal::ONE);
    negative_interruption.interruption_rate_per_hour = Some(Decimal::NEGATIVE_ONE);
    assert_eq!(
        rank(continuity, &[negative_interruption], profile),
        Err(PolicyError::InvalidObservation)
    );
    let mut invalid_failure_rate = candidate("invalid-failure-rate", Decimal::ONE);
    invalid_failure_rate.allocation_failure_rate = Some(Decimal::new(101, 2));
    assert_eq!(
        rank(continuity, &[invalid_failure_rate], profile),
        Err(PolicyError::InvalidObservation)
    );
    let mut negative_latency = candidate("negative-latency", Decimal::ONE);
    negative_latency.mean_create_seconds = Some(Decimal::NEGATIVE_ONE);
    assert_eq!(
        rank(continuity, &[negative_latency], profile),
        Err(PolicyError::InvalidObservation)
    );
    let mut reversed_window = candidate("reversed-window", Decimal::ONE);
    let window = reversed_window
        .observation_window
        .as_mut()
        .ok_or_else(|| io::Error::other("candidate observation window is missing"))?;
    window.start_unix_seconds = 2;
    window.end_unix_seconds = 1;
    assert_eq!(
        rank(continuity, &[reversed_window], profile),
        Err(PolicyError::InvalidObservation)
    );
    let mut inconsistent_samples = candidate("inconsistent-samples", Decimal::ONE);
    inconsistent_samples.observation_samples = 5;
    assert_eq!(
        rank(continuity, &[inconsistent_samples], profile),
        Err(PolicyError::InvalidObservation)
    );
    let mut insufficient_window = candidate("insufficient-window", Decimal::ONE);
    insufficient_window
        .observation_window
        .as_mut()
        .ok_or_else(|| io::Error::other("candidate observation window is missing"))?
        .minimum_samples = 1;
    assert_eq!(
        rank(continuity, &[insufficient_window], profile),
        Err(PolicyError::MissingData("adequate recent observations"))
    );
    Ok(())
}

#[test]
fn configured_missing_signal_fallback_and_cycles() {
    let mut definitions = BTreeMap::new();
    definitions.insert(
        "economy".to_owned(),
        PolicyDefinition {
            strategy: Policy::Economy {
                max_price_age_seconds: 600,
            },
            missing_data: MissingDataBehavior::Fallback {
                policy: "ordered".to_owned(),
            },
        },
    );
    definitions.insert(
        "ordered".to_owned(),
        PolicyDefinition {
            strategy: Policy::Ordered {
                identities: vec!["a".to_owned()],
            },
            missing_data: MissingDataBehavior::Reject,
        },
    );
    let mut unavailable_price = candidate("a", Decimal::ONE);
    unavailable_price.price = None;
    let mut profile = profile().unwrap();
    profile.maximum_total_cost = None;
    profile.administrator_limits.max_total_cost = None;
    let result = rank_configured("economy", &definitions, &[unavailable_price], &profile).unwrap();
    assert_eq!(result.applied_policy, "ordered");
    assert_eq!(result.fallback_from.as_deref(), Some("economy"));
    assert_eq!(result.attempts[0].outcome, "fallback");
    assert!(!result.attempts[0].candidates[0].eligible);
    assert_eq!(result.attempts[1].outcome, "applied");

    definitions.get_mut("ordered").unwrap().missing_data = MissingDataBehavior::Fallback {
        policy: "economy".to_owned(),
    };

    assert_eq!(
        rank_configured(
            "economy",
            &definitions,
            &[candidate("a", Decimal::ONE)],
            &profile
        )
        .unwrap_err(),
        PolicyError::FallbackCycle
    );
    assert_eq!(
        outrider::policy::validate_fallback_graph(&definitions).unwrap_err(),
        PolicyError::FallbackCycle
    );

    let mut rejection = BTreeMap::new();
    rejection.insert(
        "economy".to_owned(),
        PolicyDefinition {
            strategy: Policy::Economy {
                max_price_age_seconds: 600,
            },
            missing_data: MissingDataBehavior::Reject,
        },
    );
    let mut unavailable = candidate("unpriced", Decimal::ONE);
    unavailable.price = None;
    let evaluation =
        outrider::policy::evaluate_configured("economy", &rejection, &[unavailable], &profile);
    assert_eq!(evaluation.error, Some(PolicyError::MissingData("price")));
    assert_eq!(evaluation.attempts[0].outcome, "rejected");
    assert!(!evaluation.attempts[0].candidates[0].eligible);
    assert!(
        evaluation.attempts[0].candidates[0]
            .reason
            .contains("price")
    );
}
