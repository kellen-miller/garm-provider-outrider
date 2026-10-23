use std::collections::BTreeSet;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::config::RunnerProfile;

use super::ranking::rank_evaluation;
use super::{Candidate, CandidateExplanation, PolicyError};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicyDefinition {
    pub strategy: Policy,
    pub missing_data: MissingDataBehavior,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum MissingDataBehavior {
    Reject,
    Fallback { policy: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RankingResult {
    pub applied_policy: String,
    pub fallback_from: Option<String>,
    pub attempts: Vec<PolicyAttempt>,
    pub candidates: Vec<CandidateExplanation>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PolicyAttempt {
    pub name: String,
    pub outcome: String,
    pub reason: Option<String>,
    pub candidates: Vec<CandidateExplanation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyEvaluation {
    pub ranking: Option<RankingResult>,
    pub attempts: Vec<PolicyAttempt>,
    pub error: Option<PolicyError>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "strategy", rename_all = "snake_case", deny_unknown_fields)]
pub enum Policy {
    Economy {
        max_price_age_seconds: u64,
    },
    Continuity {
        max_observation_age_seconds: u64,
        min_samples: u64,
    },
    Weighted(WeightedPolicy),
    Ordered {
        identities: Vec<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WeightedPolicy {
    pub cost_weight: Decimal,
    pub failure_weight: Decimal,
    pub interruption_weight: Decimal,
    pub latency_weight: Decimal,
    pub cost_scale: Decimal,
    pub interruption_scale: Decimal,
    pub latency_scale: Decimal,
    pub min_samples: u64,
    pub max_observation_age_seconds: u64,
    pub max_price_age_seconds: u64,
}

/// Rank candidates using named policies and their missing-data fallbacks.
///
/// # Errors
///
/// Returns an error for an invalid fallback graph or when the selected policy
/// cannot rank its candidates.
pub fn rank_configured(
    initial_policy: &str,
    definitions: &std::collections::BTreeMap<String, PolicyDefinition>,
    candidates: &[Candidate],
    profile: &RunnerProfile,
) -> Result<RankingResult, PolicyError> {
    let evaluation = evaluate_configured(initial_policy, definitions, candidates, profile);
    match (evaluation.ranking, evaluation.error) {
        (Some(ranking), None) => Ok(ranking),
        (_, Some(error)) => Err(error),
        _ => Err(PolicyError::NoEligibleCandidates),
    }
}

/// Evaluate the configured policy chain and retain the real attempt trace on failure.
pub fn evaluate_configured(
    initial_policy: &str,
    definitions: &std::collections::BTreeMap<String, PolicyDefinition>,
    candidates: &[Candidate],
    profile: &RunnerProfile,
) -> PolicyEvaluation {
    if let Err(error) = validate_fallback_graph(definitions) {
        return PolicyEvaluation {
            ranking: None,
            attempts: Vec::new(),
            error: Some(error),
        };
    }
    let mut current = initial_policy;
    let mut fallback_from = None;
    let mut attempts = Vec::new();
    loop {
        let Some(definition) = definitions.get(current) else {
            return PolicyEvaluation {
                ranking: None,
                attempts,
                error: Some(PolicyError::MissingData("named policy")),
            };
        };
        let policy_result = rank_evaluation(&definition.strategy, candidates, profile);
        let ranked_candidates = policy_result.candidates;
        match (ranked_candidates, policy_result.error) {
            (candidates, None) => {
                attempts.push(PolicyAttempt {
                    name: current.to_owned(),
                    outcome: "applied".to_owned(),
                    reason: None,
                    candidates: candidates.clone(),
                });
                let ranking = RankingResult {
                    applied_policy: current.to_owned(),
                    fallback_from,
                    attempts: attempts.clone(),
                    candidates,
                };
                return PolicyEvaluation {
                    ranking: Some(ranking),
                    attempts,
                    error: None,
                };
            }

            (candidates, Some(error @ PolicyError::MissingData(_))) => {
                match &definition.missing_data {
                    MissingDataBehavior::Reject => {
                        attempts.push(PolicyAttempt {
                            name: current.to_owned(),
                            outcome: "rejected".to_owned(),
                            reason: Some(error.to_string()),
                            candidates,
                        });
                        return PolicyEvaluation {
                            ranking: None,
                            attempts,
                            error: Some(error),
                        };
                    }
                    MissingDataBehavior::Fallback { policy } => {
                        attempts.push(PolicyAttempt {
                            name: current.to_owned(),
                            outcome: "fallback".to_owned(),
                            reason: Some(error.to_string()),
                            candidates,
                        });
                        if fallback_from.is_none() {
                            fallback_from = Some(current.to_owned());
                        }

                        let Some((name, _)) = definitions.get_key_value(policy) else {
                            return PolicyEvaluation {
                                ranking: None,
                                attempts,
                                error: Some(PolicyError::MissingData("fallback policy")),
                            };
                        };
                        current = name.as_str();
                    }
                }
            }
            (candidates, Some(error)) => {
                attempts.push(PolicyAttempt {
                    name: current.to_owned(),
                    outcome: "rejected".to_owned(),
                    reason: Some(error.to_string()),
                    candidates,
                });
                return PolicyEvaluation {
                    ranking: None,
                    attempts,
                    error: Some(error),
                };
            }
        }
    }
}

/// Reject cycles or dangling references in the configured fallback graph.
///
/// # Errors
///
/// Returns an error when a fallback cycle exists or a referenced policy is
/// missing.
pub fn validate_fallback_graph(
    definitions: &std::collections::BTreeMap<String, PolicyDefinition>,
) -> Result<(), PolicyError> {
    for start in definitions.keys() {
        let mut visited = BTreeSet::new();
        let mut current = start.as_str();
        loop {
            if !visited.insert(current.to_owned()) {
                return Err(PolicyError::FallbackCycle);
            }

            let definition = definitions
                .get(current)
                .ok_or(PolicyError::MissingData("fallback policy"))?;
            match &definition.missing_data {
                MissingDataBehavior::Reject => break,
                MissingDataBehavior::Fallback { policy } => current = policy,
            }
        }
    }

    Ok(())
}
