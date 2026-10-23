use std::cmp::Ordering;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::config::RunnerProfile;

use super::strategy::WeightedPolicy;
use super::{Candidate, NativeCapacityHint, Policy, PolicyError};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ObservationDimension {
    Create,
    Interruption,
    Latency,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CandidateExplanation {
    pub identity: String,
    pub eligible: bool,
    pub reason: String,
    pub score: Option<Decimal>,
    pub secondary_score: Option<Decimal>,
    pub purchasing_tier: Option<usize>,
    pub price_age_seconds: Option<u64>,
    pub score_terms: Vec<ScoreTerm>,
    pub native_capacity_hint: Option<NativeCapacityHint>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScoreTerm {
    pub dimension: String,
    pub value: Decimal,
    pub weight: Decimal,
    pub scale: Decimal,
    pub normalized_value: Decimal,
    pub contribution: Decimal,
    pub horizon_seconds: Option<u64>,
}

/// Rank eligible candidates with a selected allocation policy.
///
/// # Errors
///
/// Returns an error for invalid observations, missing policy data, arithmetic
/// overflow, or when no candidate can be ranked.
pub fn rank(
    policy: &Policy,
    candidates: &[Candidate],
    profile: &RunnerProfile,
) -> Result<Vec<CandidateExplanation>, PolicyError> {
    let evaluation = rank_evaluation(policy, candidates, profile);
    match (evaluation.candidates, evaluation.error) {
        (candidates, None) => Ok(candidates),
        (_, Some(error)) => Err(error),
    }
}

pub(super) struct PolicyRankEvaluation {
    pub(super) candidates: Vec<CandidateExplanation>,
    pub(super) error: Option<PolicyError>,
}

struct PolicyScore {
    score: Option<Decimal>,
    secondary_score: Option<Decimal>,
    reason: String,
    terms: Vec<ScoreTerm>,
}

pub(super) fn rank_evaluation(
    policy: &Policy,
    candidates: &[Candidate],
    profile: &RunnerProfile,
) -> PolicyRankEvaluation {
    if candidates.is_empty() {
        return PolicyRankEvaluation {
            candidates: Vec::new(),
            error: Some(PolicyError::NoEligibleCandidates),
        };
    }

    if let Policy::Weighted(weighted) = policy
        && let Err(error) = validate_weighted(weighted)
    {
        return PolicyRankEvaluation {
            candidates: Vec::new(),
            error: Some(error),
        };
    }

    let mut explanations = Vec::with_capacity(candidates.len());
    let mut first_error = None;
    for candidate in candidates {
        if let Err(error) = validate_observed_inputs(candidate) {
            first_error.get_or_insert(error);
            explanations.push(CandidateExplanation {
                identity: candidate.identity.clone(),
                eligible: false,
                reason: "candidate observation data is invalid".to_owned(),
                score: None,
                secondary_score: None,
                purchasing_tier: None,
                price_age_seconds: candidate
                    .price
                    .as_ref()
                    .and_then(|price| price.observed_age_seconds),
                score_terms: Vec::new(),
                native_capacity_hint: candidate.native_capacity_hint.clone(),
            });
            continue;
        }
        match candidate.meets(profile) {
            Err(reason) => explanations.push(CandidateExplanation {
                identity: candidate.identity.clone(),
                eligible: false,
                reason: reason.to_owned(),
                score: None,
                secondary_score: None,
                purchasing_tier: None,
                price_age_seconds: candidate
                    .price
                    .as_ref()
                    .and_then(|p| p.observed_age_seconds),
                score_terms: Vec::new(),
                native_capacity_hint: candidate.native_capacity_hint.clone(),
            }),
            Ok(()) => {
                let maximum_total_cost = match (
                    profile.maximum_total_cost,
                    profile.administrator_limits.max_total_cost,
                ) {
                    (Some(user), Some(administrator)) => Some(user.min(administrator)),
                    (Some(user), None) => Some(user),
                    (None, Some(administrator)) => Some(administrator),
                    (None, None) => None,
                };
                if let Some(maximum) = maximum_total_cost {
                    let Some(quote) = candidate.price.as_ref() else {
                        first_error
                            .get_or_insert(PolicyError::MissingData("price for cost ceiling"));
                        explanations.push(ineligible(candidate, "missing price for cost ceiling"));
                        continue;
                    };
                    if quote
                        .observed_age_seconds
                        .is_none_or(|age| age > profile.max_price_age_seconds)
                    {
                        first_error.get_or_insert(PolicyError::MissingData(
                            "fresh price for cost ceiling",
                        ));
                        explanations.push(ineligible(
                            candidate,
                            "missing fresh price for cost ceiling",
                        ));
                        continue;
                    }

                    let total = match quote.estimated_total(profile.cost_horizon_seconds) {
                        Ok(total) => total,
                        Err(error) => {
                            first_error.get_or_insert(error.clone());
                            explanations.push(ineligible(candidate, &error.to_string()));
                            continue;
                        }
                    };
                    if total > maximum {
                        explanations.push(CandidateExplanation {
                            identity: candidate.identity.clone(),
                            eligible: false,
                            reason: "above maximum total cost".to_owned(),
                            score: None,
                            secondary_score: None,
                            purchasing_tier: None,
                            price_age_seconds: quote.observed_age_seconds,
                            score_terms: Vec::new(),
                            native_capacity_hint: candidate.native_capacity_hint.clone(),
                        });
                        continue;
                    }
                }

                let scored = match score(policy, candidate, profile.cost_horizon_seconds) {
                    Ok(score) => score,
                    Err(error) => {
                        first_error.get_or_insert(error.clone());
                        explanations.push(ineligible(candidate, &error.to_string()));
                        continue;
                    }
                };
                explanations.push(CandidateExplanation {
                    identity: candidate.identity.clone(),
                    eligible: true,
                    reason: scored.reason,
                    score: scored.score,
                    secondary_score: scored.secondary_score,
                    purchasing_tier: profile
                        .purchasing_tiers
                        .iter()
                        .position(|tier| tier.contains(&candidate.purchasing)),
                    price_age_seconds: candidate
                        .price
                        .as_ref()
                        .and_then(|p| p.observed_age_seconds),
                    score_terms: scored.terms,
                    native_capacity_hint: candidate.native_capacity_hint.clone(),
                });
            }
        }
    }

    explanations.sort_by(|a, b| match (a.eligible, b.eligible) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => a.identity.cmp(&b.identity),
        (true, true) => a.purchasing_tier.cmp(&b.purchasing_tier).then_with(|| {
            a.score
                .cmp(&b.score)
                .then_with(|| a.secondary_score.cmp(&b.secondary_score))
                .then_with(|| a.identity.cmp(&b.identity))
        }),
    });
    PolicyRankEvaluation {
        candidates: explanations,
        error: first_error,
    }
}

fn ineligible(candidate: &Candidate, reason: &str) -> CandidateExplanation {
    CandidateExplanation {
        identity: candidate.identity.clone(),
        eligible: false,
        reason: reason.to_owned(),
        score: None,
        secondary_score: None,
        purchasing_tier: None,
        price_age_seconds: candidate
            .price
            .as_ref()
            .and_then(|price| price.observed_age_seconds),
        score_terms: Vec::new(),
        native_capacity_hint: candidate.native_capacity_hint.clone(),
    }
}

fn score(
    policy: &Policy,
    candidate: &Candidate,
    cost_horizon_seconds: u64,
) -> Result<PolicyScore, PolicyError> {
    if let Policy::Weighted(weights) = policy {
        return score_weighted(candidate, weights, cost_horizon_seconds);
    }
    let (score, secondary_score, reason) = match policy {
        Policy::Economy {
            max_price_age_seconds,
        } => score_economy(candidate, cost_horizon_seconds, *max_price_age_seconds)?,
        Policy::Continuity {
            max_observation_age_seconds,
            min_samples,
        } => score_continuity(candidate, *min_samples, *max_observation_age_seconds)?,
        Policy::Ordered { identities } => score_ordered(candidate, identities)?,
        Policy::Weighted(_) => unreachable!(),
    };
    Ok(PolicyScore {
        score,
        secondary_score,
        reason,
        terms: Vec::new(),
    })
}

fn score_economy(
    candidate: &Candidate,
    cost_horizon_seconds: u64,
    max_price_age_seconds: u64,
) -> Result<(Option<Decimal>, Option<Decimal>, String), PolicyError> {
    let price = candidate
        .price
        .as_ref()
        .ok_or(PolicyError::MissingData("price"))?;
    if price
        .observed_age_seconds
        .is_none_or(|age| age > max_price_age_seconds)
    {
        return Err(PolicyError::MissingData("fresh price"));
    }

    let total = price.estimated_total(cost_horizon_seconds)?;
    Ok((
        Some(total),
        None,
        format!(
            "estimated total {} {total} over {cost_horizon_seconds}s",
            price.currency
        ),
    ))
}

fn score_continuity(
    candidate: &Candidate,
    min_samples: u64,
    max_observation_age_seconds: u64,
) -> Result<(Option<Decimal>, Option<Decimal>, String), PolicyError> {
    require_observations(
        candidate,
        min_samples,
        max_observation_age_seconds,
        &[
            ObservationDimension::Create,
            ObservationDimension::Interruption,
        ],
    )?;
    let interruption = candidate
        .interruption_rate_per_hour
        .ok_or(PolicyError::MissingData("interruption rate"))?;
    let failures = candidate
        .allocation_failure_rate
        .ok_or(PolicyError::MissingData("allocation failure rate"))?;
    Ok((
        Some(interruption),
        Some(failures),
        format!("interruption rate {interruption}; definitive allocation failure rate {failures}"),
    ))
}

fn score_weighted(
    candidate: &Candidate,
    weights: &WeightedPolicy,
    cost_horizon_seconds: u64,
) -> Result<PolicyScore, PolicyError> {
    let mut total = Decimal::ZERO;
    let mut active = Vec::new();
    let mut terms = Vec::new();
    if weights.cost_weight > Decimal::ZERO {
        let price = candidate
            .price
            .as_ref()
            .ok_or(PolicyError::MissingData("price"))?;
        if price
            .observed_age_seconds
            .is_none_or(|age| age > weights.max_price_age_seconds)
        {
            return Err(PolicyError::MissingData("fresh price"));
        }

        let value = price.estimated_total(cost_horizon_seconds)?;
        add_weighted_score(
            &mut total,
            &mut active,
            &mut terms,
            WeightedScoreInput {
                name: "cost",
                value,
                weight: weights.cost_weight,
                scale: weights.cost_scale,
                horizon_seconds: Some(cost_horizon_seconds),
            },
        )?;
    }

    if weights.failure_weight > Decimal::ZERO
        || weights.interruption_weight > Decimal::ZERO
        || weights.latency_weight > Decimal::ZERO
    {
        let mut required = Vec::new();
        if weights.failure_weight > Decimal::ZERO {
            required.push(ObservationDimension::Create);
        }
        if weights.interruption_weight > Decimal::ZERO {
            required.push(ObservationDimension::Interruption);
        }
        if weights.latency_weight > Decimal::ZERO {
            required.push(ObservationDimension::Latency);
        }
        require_observations(
            candidate,
            weights.min_samples,
            weights.max_observation_age_seconds,
            &required,
        )?;
    }

    if weights.failure_weight > Decimal::ZERO {
        add_weighted_score(
            &mut total,
            &mut active,
            &mut terms,
            WeightedScoreInput {
                name: "failure",
                value: candidate
                    .allocation_failure_rate
                    .ok_or(PolicyError::MissingData("allocation failure rate"))?,
                weight: weights.failure_weight,
                scale: Decimal::ONE,
                horizon_seconds: None,
            },
        )?;
    }

    if weights.interruption_weight > Decimal::ZERO {
        add_weighted_score(
            &mut total,
            &mut active,
            &mut terms,
            WeightedScoreInput {
                name: "interruption",
                value: candidate
                    .interruption_rate_per_hour
                    .ok_or(PolicyError::MissingData("interruption rate"))?,
                weight: weights.interruption_weight,
                scale: weights.interruption_scale,
                horizon_seconds: None,
            },
        )?;
    }

    if weights.latency_weight > Decimal::ZERO {
        add_weighted_score(
            &mut total,
            &mut active,
            &mut terms,
            WeightedScoreInput {
                name: "latency",
                value: candidate
                    .mean_create_seconds
                    .ok_or(PolicyError::MissingData("create latency"))?,
                weight: weights.latency_weight,
                scale: weights.latency_scale,
                horizon_seconds: None,
            },
        )?;
    }

    Ok(PolicyScore {
        score: Some(total),
        secondary_score: None,
        reason: format!("weighted score {total}: {}", active.join(", ")),
        terms,
    })
}

struct WeightedScoreInput {
    name: &'static str,
    value: Decimal,
    weight: Decimal,
    scale: Decimal,
    horizon_seconds: Option<u64>,
}

fn add_weighted_score(
    total: &mut Decimal,
    active: &mut Vec<String>,
    terms: &mut Vec<ScoreTerm>,
    input: WeightedScoreInput,
) -> Result<(), PolicyError> {
    let normalized = input
        .value
        .checked_div(input.scale)
        .ok_or(PolicyError::CostOverflow)?;
    let weighted = normalized
        .checked_mul(input.weight)
        .ok_or(PolicyError::CostOverflow)?;
    *total = total
        .checked_add(weighted)
        .ok_or(PolicyError::CostOverflow)?;
    terms.push(ScoreTerm {
        dimension: input.name.to_owned(),
        value: input.value,
        weight: input.weight,
        scale: input.scale,
        normalized_value: normalized,
        contribution: weighted,
        horizon_seconds: input.horizon_seconds,
    });
    let suffix = input
        .horizon_seconds
        .map_or_else(String::new, |seconds| format!(" over {seconds}s"));
    active.push(format!("{}={normalized}{suffix}", input.name));
    Ok(())
}

fn score_ordered(
    candidate: &Candidate,
    identities: &[String],
) -> Result<(Option<Decimal>, Option<Decimal>, String), PolicyError> {
    let index = identities
        .iter()
        .position(|identity| identity == &candidate.identity)
        .ok_or(PolicyError::MissingData("ordered selector"))?;
    Ok((
        Some(Decimal::from(index as u64)),
        None,
        format!("selector position {}", index + 1),
    ))
}

fn require_observations(
    candidate: &Candidate,
    minimum: u64,
    max_age: u64,
    dimensions: &[ObservationDimension],
) -> Result<(), PolicyError> {
    let ages = candidate.observation_ages.as_ref();
    if dimensions.iter().any(|dimension| {
        let age = match dimension {
            ObservationDimension::Create => ages.and_then(|ages| ages.create_seconds),
            ObservationDimension::Interruption => ages.and_then(|ages| ages.interruption_seconds),
            ObservationDimension::Latency => ages.and_then(|ages| ages.latency_seconds),
        };
        age.is_none_or(|age| age > max_age)
    }) || candidate.observation_window.as_ref().is_none_or(|window| {
        window.start_unix_seconds > window.end_unix_seconds || window.minimum_samples < minimum
    }) || candidate.observation_samples < minimum
        || (dimensions.contains(&ObservationDimension::Interruption)
            && candidate.interruption_rate_per_hour.is_some()
            && candidate.interruption_samples < minimum)
        || (dimensions.contains(&ObservationDimension::Create)
            && candidate.allocation_failure_rate.is_some()
            && candidate.definitive_create_outcomes < minimum)
        || (dimensions.contains(&ObservationDimension::Latency)
            && candidate.measured_latency_samples < minimum)
    {
        return Err(PolicyError::MissingData("adequate recent observations"));
    }

    Ok(())
}

fn validate_observed_inputs(candidate: &Candidate) -> Result<(), PolicyError> {
    if candidate
        .interruption_rate_per_hour
        .is_some_and(|rate| rate < Decimal::ZERO)
        || candidate
            .allocation_failure_rate
            .is_some_and(|rate| !(Decimal::ZERO..=Decimal::ONE).contains(&rate))
        || candidate
            .mean_create_seconds
            .is_some_and(|latency| latency < Decimal::ZERO)
        || candidate.observation_window.as_ref().is_some_and(|window| {
            window.start_unix_seconds > window.end_unix_seconds
                || window.minimum_samples == 0
                || candidate.observation_samples < window.minimum_samples
        })
    {
        return Err(PolicyError::InvalidObservation);
    }

    Ok(())
}

fn validate_weighted(policy: &WeightedPolicy) -> Result<(), PolicyError> {
    let weights = [
        &policy.cost_weight,
        &policy.failure_weight,
        &policy.interruption_weight,
        &policy.latency_weight,
    ];
    if weights.iter().any(|weight| **weight < Decimal::ZERO)
        || weights.iter().all(|weight| **weight == Decimal::ZERO)
        || policy.cost_scale <= Decimal::ZERO
        || policy.interruption_scale <= Decimal::ZERO
        || policy.latency_scale <= Decimal::ZERO
    {
        return Err(PolicyError::InvalidWeightedPolicy);
    }

    Ok(())
}
