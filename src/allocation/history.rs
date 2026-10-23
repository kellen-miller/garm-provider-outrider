use super::selection::PolicyRequirements;
use super::{
    AllocationError, AllocationService, BTreeMap, BTreeSet, CatalogOffer, ConfigPatch,
    ExplanationHistoryQueryWindow, HashMap, Journal, ProviderBinding, ProviderLaunchFacts,
    ProviderSelection, RunnerProfile, unix_now_millis,
};
use crate::providers::provider_pin_json;
use sha2::Digest;

pub(super) struct CandidateHistoryAnalysis {
    pub(super) unknown_reasons: HashMap<String, Vec<String>>,
    pub(super) query_window: Option<ExplanationHistoryQueryWindow>,
}
pub(super) struct ExplanationHistoryContext<'a> {
    pub(super) unknown_reasons: &'a [String],
    pub(super) query_window: Option<&'a ExplanationHistoryQueryWindow>,
}
#[derive(serde::Serialize)]
struct CandidateHistoryMaterial<'a> {
    candidate: crate::policy::Candidate,
    selection: &'a ProviderSelection,
    launch_facts: &'a ProviderLaunchFacts,
    catalog_fingerprint: &'a str,
    binding_id: &'a str,
    source_revision: &'a str,
    executable_sha256: &'a str,
    configuration_sha256: &'a str,
}
#[derive(Default)]
pub(super) struct UnknownCreateCoverage {
    pub(super) counts: BTreeMap<UnknownCreateScope, u64>,
    pub(super) incomplete_scope: bool,
}
#[derive(Ord, PartialOrd, Eq, PartialEq)]
pub(super) struct UnknownCreateScope {
    pub(super) candidate_key: String,
    pub(super) catalog_fingerprint: String,
    pub(super) provider_pin_json: String,
}

pub(super) fn latest_policy_observation(
    policy: &crate::policy::Policy,
    history: &crate::journal::CandidateHistory,
) -> Option<u128> {
    let dimensions = match policy {
        crate::policy::Policy::Continuity { .. } => vec![
            history.latest_create_millis,
            history.latest_observation_millis,
        ],
        crate::policy::Policy::Weighted(policy) => {
            let mut dimensions = Vec::new();
            if policy.failure_weight > rust_decimal::Decimal::ZERO {
                dimensions.push(history.latest_create_millis);
            }
            if policy.latency_weight > rust_decimal::Decimal::ZERO {
                dimensions.push(history.latest_latency_millis);
            }
            if policy.interruption_weight > rust_decimal::Decimal::ZERO {
                dimensions.push(history.latest_observation_millis);
            }
            dimensions
        }
        crate::policy::Policy::Economy { .. } | crate::policy::Policy::Ordered { .. } => {
            return None;
        }
    };
    if dimensions.is_empty() || dimensions.iter().any(Option::is_none) {
        return None;
    }
    dimensions.into_iter().flatten().min()
}

pub(super) fn unknown_create_coverage(
    journal: &Journal,
    fingerprints: &BTreeSet<String>,
    window_start_millis: u128,
    window_end_millis: u128,
) -> Result<UnknownCreateCoverage, AllocationError> {
    let attempts = journal
        .unknown_create_attempts(window_start_millis, window_end_millis)
        .map_err(|_| AllocationError::Journal)?;
    let mut coverage = UnknownCreateCoverage::default();
    for attempt in attempts {
        let Some(fingerprint) = attempt.catalog_fingerprint.as_deref() else {
            coverage.incomplete_scope = true;
            continue;
        };
        if !fingerprints.contains(fingerprint) {
            continue;
        }
        if attempt.observed_at_millis.is_none() {
            coverage.incomplete_scope = true;
            continue;
        }
        let (Ok(offer), Ok(binding)) = (
            serde_json::from_str::<CatalogOffer>(&attempt.offer_json),
            serde_json::from_str::<ProviderBinding>(&attempt.binding_json),
        ) else {
            coverage.incomplete_scope = true;
            continue;
        };
        let scope = UnknownCreateScope {
            candidate_key: candidate_history_key(&offer, fingerprint, &binding)?,
            catalog_fingerprint: fingerprint.to_owned(),
            provider_pin_json: provider_pin_json(&binding),
        };
        *coverage.counts.entry(scope).or_insert(0) += 1;
    }
    Ok(coverage)
}

impl crate::journal::CandidateHistory {
    fn mean_create_seconds(&self) -> Result<Option<rust_decimal::Decimal>, AllocationError> {
        if self.create_latencies_nanos.is_empty() {
            return Ok(None);
        }
        let total_seconds = self
            .create_latencies_nanos
            .iter()
            .filter_map(|nanos| {
                rust_decimal::Decimal::from(*nanos)
                    .checked_div(rust_decimal::Decimal::from(1_000_000_000u64))
            })
            .try_fold(
                rust_decimal::Decimal::ZERO,
                rust_decimal::Decimal::checked_add,
            )
            .ok_or(AllocationError::Journal)?;
        let mean = total_seconds
            .checked_div(rust_decimal::Decimal::from(self.measured_latency_samples))
            .ok_or(AllocationError::Journal)?;
        Ok(Some(mean))
    }
}

impl AllocationService {
    pub(super) fn collect_candidate_history(
        &self,
        offers: &mut [(CatalogOffer, String)],
        profile: &RunnerProfile,
        central: &ConfigPatch,
        fingerprints: &BTreeSet<String>,
        requirements: &PolicyRequirements,
    ) -> Result<CandidateHistoryAnalysis, AllocationError> {
        let mut history_unknown_reasons = HashMap::new();
        let Some(settings) = &central.history else {
            return Ok(CandidateHistoryAnalysis {
                unknown_reasons: history_unknown_reasons,
                query_window: None,
            });
        };
        let now = unix_now_millis();
        let window_millis = u128::from(settings.window_seconds) * 1000;
        let window_start = now.saturating_sub(window_millis);
        let unknown_create_coverage = if requirements.requires_create_failure {
            unknown_create_coverage(&self.journal, fingerprints, window_start, now)?
        } else {
            UnknownCreateCoverage::default()
        };
        let policies = central
            .policies
            .as_ref()
            .ok_or(AllocationError::Configuration)?;
        let active_policy = policies
            .get(&profile.policy)
            .map(|definition| &definition.strategy);
        for (offer, fingerprint) in offers {
            let binding = central
                .provider_bindings
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|binding| binding.id == offer.selection.binding_id)
                .ok_or(AllocationError::Configuration)?;
            let candidate_key = candidate_history_key(offer, fingerprint, binding)?;
            let binding_pin = provider_pin_json(binding);
            let history = self
                .journal
                .candidate_history(
                    &candidate_key,
                    fingerprint,
                    &binding_pin,
                    window_start,
                    now,
                    settings.maximum_gap_seconds.saturating_mul(1000),
                )
                .map_err(|_| AllocationError::Journal)?;
            let create_count = history.definitive_create_outcomes;
            let unknown_create_count = unknown_create_coverage
                .counts
                .get(&UnknownCreateScope {
                    candidate_key,
                    catalog_fingerprint: fingerprint.clone(),
                    provider_pin_json: binding_pin,
                })
                .copied()
                .unwrap_or_default();
            let mut reasons = Vec::new();
            if requirements.requires_create_failure && unknown_create_count > 0 {
                reasons.push(format!(
                    "{unknown_create_count} dispatched create outcome(s) are unresolved"
                ));
            }
            if requirements.requires_create_failure && unknown_create_coverage.incomplete_scope {
                reasons.push(
                    "create outcome coverage is incomplete for this catalog scope".to_owned(),
                );
            }
            if requirements.requires_create_failure
                && create_count == 0
                && unknown_create_count == 0
            {
                reasons.push(
                    "no qualified create outcomes were measured in the history window".to_owned(),
                );
            }
            if requirements.requires_latency && !history.latency_coverage_complete {
                reasons.push("latency history is incomplete for the requested window".to_owned());
            }
            if requirements.requires_latency && history.measured_latency_samples == 0 {
                reasons.push(
                    "no measured create latency is available in the history window".to_owned(),
                );
            }
            if requirements.requires_interruption_exposure && !history.running_coverage_complete {
                reasons.push(
                    "running exposure history is incomplete for the requested window".to_owned(),
                );
            }
            if requirements.requires_interruption_exposure && history.running_exposure_millis == 0 {
                reasons.push(
                    "no verified running interval is available in the history window".to_owned(),
                );
            }
            if !reasons.is_empty() {
                history_unknown_reasons.insert(offer.candidate.identity.clone(), reasons);
            }
            offer.candidate.definitive_create_outcomes = create_count;
            offer.candidate.measured_latency_samples = history.measured_latency_samples;
            offer.candidate.allocation_failure_rate = (requirements.requires_create_failure
                && create_count > 0
                && unknown_create_count == 0
                && !unknown_create_coverage.incomplete_scope)
                .then_some(rust_decimal::Decimal::ZERO);
            offer.candidate.interruption_samples = history.observation_samples;
            offer.candidate.observation_samples = create_count
                .max(history.observation_samples)
                .max(history.interruptions);
            offer.candidate.mean_create_seconds = history.mean_create_seconds()?;
            let exposure_hours = rust_decimal::Decimal::from(history.running_exposure_millis)
                .checked_div(rust_decimal::Decimal::from(3_600_000u64))
                .ok_or(AllocationError::Journal)?;
            offer.candidate.interruption_rate_per_hour =
                if exposure_hours > rust_decimal::Decimal::ZERO {
                    rust_decimal::Decimal::from(history.interruptions).checked_div(exposure_hours)
                } else {
                    None
                };
            let latest =
                active_policy.and_then(|policy| latest_policy_observation(policy, &history));
            offer.candidate.observation_age_seconds = latest
                .map(|latest| now.saturating_sub(latest) / 1000)
                .and_then(|age| u64::try_from(age).ok());
            offer.candidate.observation_ages = Some(crate::policy::ObservationAges {
                create_seconds: history
                    .latest_create_millis
                    .map(|latest| now.saturating_sub(latest) / 1000)
                    .and_then(|age| u64::try_from(age).ok()),
                latency_seconds: history
                    .latest_latency_millis
                    .map(|latest| now.saturating_sub(latest) / 1000)
                    .and_then(|age| u64::try_from(age).ok()),
                interruption_seconds: history
                    .latest_observation_millis
                    .map(|latest| now.saturating_sub(latest) / 1000)
                    .and_then(|age| u64::try_from(age).ok()),
            });
            offer.candidate.observation_window = (offer.candidate.observation_samples > 0)
                .then_some(crate::policy::ObservationWindow {
                    start_unix_seconds: u64::try_from(window_start / 1000).unwrap_or(u64::MAX),
                    end_unix_seconds: u64::try_from(now / 1000).unwrap_or(u64::MAX),
                    minimum_samples: offer.candidate.observation_samples,
                });
        }
        Ok(CandidateHistoryAnalysis {
            unknown_reasons: history_unknown_reasons,
            query_window: Some(ExplanationHistoryQueryWindow {
                start_unix_seconds: u64::try_from(window_start / 1000).unwrap_or(u64::MAX),
                end_unix_seconds: u64::try_from(now / 1000).unwrap_or(u64::MAX),
            }),
        })
    }
}

pub(super) fn candidate_history_key(
    offer: &CatalogOffer,
    catalog_fingerprint: &str,
    binding: &ProviderBinding,
) -> Result<String, AllocationError> {
    let mut candidate = offer.candidate.clone();
    candidate.identity.clear();
    candidate.price = None;
    candidate.native_capacity_hint = None;
    candidate.interruption_rate_per_hour = None;
    candidate.allocation_failure_rate = None;
    candidate.mean_create_seconds = None;
    candidate.observation_age_seconds = None;
    candidate.observation_ages = None;
    candidate.observation_window = None;
    candidate.observation_samples = 0;
    candidate.definitive_create_outcomes = 0;
    candidate.measured_latency_samples = 0;
    candidate.interruption_samples = 0;
    let material = CandidateHistoryMaterial {
        candidate,
        selection: &offer.selection,
        launch_facts: &offer.launch_facts,
        catalog_fingerprint,
        binding_id: &binding.id,
        source_revision: &binding.source_revision,
        executable_sha256: &binding.executable_sha256,
        configuration_sha256: &binding.configuration_sha256,
    };
    let bytes = serde_json::to_vec(&material).map_err(|_| AllocationError::Journal)?;
    Ok(format!("{:x}", sha2::Sha256::digest(bytes)))
}
