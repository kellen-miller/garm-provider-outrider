use super::explain::apply_effective_prices;
use super::explain::bounded_text;
use super::explain::explanation_financial_scope;
use super::{
    AllocationError, AllocationService, BTreeSet, CatalogOffer, ConfigPatch, Duration,
    ExplanationCatalogExclusion, ExplanationCatalogScope, ExplanationHistoryQueryWindow, HashMap,
    Instant, PoolSnapshot, PoolSnapshotId, ProviderBinding, ProviderRequest, ProviderRequirements,
    ProviderSelection, PurchasingCapability, RankingResult, RunnerProfile, SnapshotDocument,
    unix_now,
};

pub(super) struct SharedAnalysis {
    pub(super) analyzed_at_unix_seconds: u64,
    pub(super) offers: Vec<(CatalogOffer, String)>,
    pub(super) ranking: Option<RankingResult>,
    pub(super) policy_attempts: Vec<crate::policy::PolicyAttempt>,
    pub(super) policy_error: Option<String>,
    pub(super) quote_missing: bool,
    pub(super) observed_exclusions: Vec<ExplanationCatalogExclusion>,
    pub(super) queried_catalogs: Vec<ExplanationCatalogScope>,
    pub(super) history_unknown_reasons: HashMap<String, Vec<String>>,
    pub(super) history_query_window: Option<ExplanationHistoryQueryWindow>,
}

pub(super) struct PolicyRequirements {
    pub(super) maximum_price_age: Duration,
    pub(super) requires_create_failure: bool,
    pub(super) requires_latency: bool,
    pub(super) requires_interruption_exposure: bool,
}

fn policy_requirements(
    profile: &RunnerProfile,
    central: &ConfigPatch,
) -> Result<PolicyRequirements, AllocationError> {
    let policies = central
        .policies
        .as_ref()
        .ok_or(AllocationError::Configuration)?;
    let mut requirements = PolicyRequirements {
        maximum_price_age: Duration::from_secs(profile.max_price_age_seconds),
        requires_create_failure: false,
        requires_latency: false,
        requires_interruption_exposure: false,
    };
    let mut active_policy = profile.policy.as_str();
    let mut visited_policies = BTreeSet::new();
    while visited_policies.insert(active_policy.to_owned()) {
        let definition = policies
            .get(active_policy)
            .ok_or(AllocationError::Configuration)?;
        requirements.requires_create_failure |= match &definition.strategy {
            crate::policy::Policy::Continuity { .. } => true,
            crate::policy::Policy::Weighted(policy) => {
                policy.failure_weight > rust_decimal::Decimal::ZERO
            }
            _ => false,
        };
        requirements.requires_interruption_exposure |= matches!(
            &definition.strategy,
            crate::policy::Policy::Continuity { .. }
        );
        if let crate::policy::Policy::Weighted(policy) = &definition.strategy {
            requirements.requires_latency |= policy.latency_weight > rust_decimal::Decimal::ZERO;
            requirements.requires_interruption_exposure |=
                policy.interruption_weight > rust_decimal::Decimal::ZERO;
        }
        let policy_age = match &definition.strategy {
            crate::policy::Policy::Economy {
                max_price_age_seconds,
            } => Some(*max_price_age_seconds),
            crate::policy::Policy::Weighted(policy)
                if policy.cost_weight > rust_decimal::Decimal::ZERO =>
            {
                Some(policy.max_price_age_seconds)
            }
            _ => None,
        };
        if let Some(policy_age) = policy_age {
            requirements.maximum_price_age = requirements
                .maximum_price_age
                .min(Duration::from_secs(policy_age));
        }
        match &definition.missing_data {
            crate::policy::MissingDataBehavior::Reject => break,
            crate::policy::MissingDataBehavior::Fallback { policy } => {
                active_policy = policy;
            }
        }
    }
    Ok(requirements)
}

impl AllocationService {
    pub(super) async fn rank_offers(
        &self,
        profile: &RunnerProfile,
        central: &ConfigPatch,
        fingerprints: &BTreeSet<String>,
        deadline: Instant,
    ) -> Result<SharedAnalysis, AllocationError> {
        let mut offers = Vec::new();
        let mut observed_exclusions = Vec::new();
        let mut queried_catalogs = Vec::new();
        let mut quote_missing = false;
        let requirements = policy_requirements(profile, central)?;
        for catalog in self
            .catalogs
            .iter()
            .filter(|catalog| fingerprints.contains(&catalog.configuration_fingerprint))
        {
            let discovery = catalog
                .catalog(profile.clone(), deadline)
                .await
                .map_err(|_| AllocationError::Catalog)?;
            let selected_machine_types = profile
                .machine_types
                .get(catalog.cloud())
                .cloned()
                .unwrap_or_default();
            let locations = catalog.requested_locations(central, profile);
            let configured_locations = locations
                .iter()
                .map(|location| location.configured.clone())
                .collect::<Vec<_>>();
            let native_locations = locations
                .iter()
                .map(|location| location.native.clone())
                .collect::<Vec<_>>();
            let purchasing = profile
                .purchasing_models
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            queried_catalogs.push(ExplanationCatalogScope {
                cloud: bounded_text(&catalog.cloud, 32),
                account: bounded_text(&catalog.account_id, 128),
                text_truncated: catalog.cloud.chars().count() > 32
                    || catalog.account_id.chars().count() > 128
                    || configured_locations
                        .iter()
                        .chain(&native_locations)
                        .any(|value| value.chars().count() > 128)
                    || selected_machine_types
                        .iter()
                        .any(|value| value.chars().count() > 128)
                    || profile.image.chars().count() > 128
                    || purchasing.iter().any(|value| value.chars().count() > 32),
                location_count: configured_locations.len(),
                locations_truncated: configured_locations.len() > 64,
                native_location_count: native_locations.len(),
                native_locations_truncated: native_locations.len() > 64,
                all_configured_locations: profile.locations.is_empty(),
                locations: configured_locations
                    .iter()
                    .take(64)
                    .map(|value| bounded_text(value, 128))
                    .collect(),
                native_locations: native_locations
                    .iter()
                    .take(64)
                    .map(|value| bounded_text(value, 128))
                    .collect(),
                machine_type_count: selected_machine_types.len(),
                machine_types_truncated: selected_machine_types.len() > 64,
                machine_types: selected_machine_types
                    .iter()
                    .take(64)
                    .map(|value| bounded_text(value, 128))
                    .collect(),
                image_alias: bounded_text(&profile.image, 128),
                purchasing_count: purchasing.len(),
                purchasing_truncated: purchasing.len() > 16,
                purchasing: purchasing
                    .iter()
                    .take(16)
                    .map(|value| bounded_text(value, 32))
                    .collect(),
                queried: discovery.queried,
            });
            observed_exclusions.extend(discovery.exclusions.into_iter().map(|exclusion| {
                ExplanationCatalogExclusion {
                    catalog: bounded_text(&catalog.catalog_id, 128),
                    reason: bounded_text(&exclusion.reason, 256),
                    text_truncated: catalog.catalog_id.chars().count() > 128
                        || exclusion.reason.chars().count() > 256,
                    observed_count: exclusion.observed_count,
                }
            }));
            for mut offer in discovery.offers {
                offer.candidate.price = match catalog
                    .quote(
                        &offer,
                        Duration::from_secs(profile.cost_horizon_seconds),
                        requirements.maximum_price_age,
                        deadline,
                    )
                    .await
                {
                    Ok(quote) => Some(quote),
                    Err(crate::cloud::CloudError::PriceUnavailable(_)) => {
                        quote_missing = true;
                        None
                    }
                    Err(_) => return Err(AllocationError::Catalog),
                };
                offers.push((offer, catalog.configuration_fingerprint.clone()));
            }
        }
        let history = self.collect_candidate_history(
            &mut offers,
            profile,
            central,
            fingerprints,
            &requirements,
        )?;
        let analyzed_at = unix_now();
        let effective_prices = central.effective_prices.as_deref().unwrap_or_default();
        for (offer, fingerprint) in &mut offers {
            let scope = if effective_prices.is_empty() {
                None
            } else {
                Some(explanation_financial_scope(offer, central, fingerprint)?.0)
            };
            if let Some(quote) = &mut offer.candidate.price {
                if let Some(scope) = &scope {
                    apply_effective_prices(quote, scope, effective_prices, analyzed_at)?;
                }
                quote.refresh_observed_age();
            }
        }
        let candidates = offers
            .iter()
            .map(|(offer, _fingerprint)| offer.candidate.clone())
            .collect::<Vec<_>>();
        let policies = central
            .policies
            .as_ref()
            .ok_or(AllocationError::Configuration)?;
        let evaluation =
            crate::policy::evaluate_configured(&profile.policy, policies, &candidates, profile);
        if let Some(explanations) = evaluation
            .ranking
            .as_ref()
            .map(|ranking| ranking.candidates.as_slice())
            .or_else(|| {
                evaluation
                    .attempts
                    .last()
                    .map(|attempt| attempt.candidates.as_slice())
            })
        {
            let positions = explanations
                .iter()
                .enumerate()
                .map(|(index, candidate)| (candidate.identity.as_str(), index))
                .collect::<HashMap<_, _>>();
            offers.sort_by_key(|(offer, _)| {
                positions
                    .get(offer.candidate.identity.as_str())
                    .copied()
                    .unwrap_or(usize::MAX)
            });
        }
        Ok(SharedAnalysis {
            analyzed_at_unix_seconds: analyzed_at,
            offers,
            ranking: evaluation.ranking,
            policy_attempts: evaluation.attempts,
            policy_error: evaluation.error.map(|error| error.to_string()),
            quote_missing,
            observed_exclusions,
            queried_catalogs,
            history_unknown_reasons: history.unknown_reasons,
            history_query_window: history.query_window,
        })
    }

    pub(super) fn check_controller_and_pool_caps(
        &self,
        controller_id: &str,
        snapshot: &PoolSnapshot,
    ) -> Result<(), AllocationError> {
        let (controller_count, pool_count) = self
            .journal
            .active_allocation_counts(
                controller_id,
                snapshot.garm_pool_id.as_deref().unwrap_or_default(),
            )
            .map_err(|_| AllocationError::Journal)?;
        if controller_count > self.limits.max_allocations_controller as usize
            || pool_count > self.limits.max_allocations_pool as usize
        {
            return Err(AllocationError::Limit);
        }
        Ok(())
    }

    pub(super) fn check_account_cap(
        &self,
        controller_id: &str,
        snapshot: &PoolSnapshot,
        account: &str,
    ) -> Result<(), AllocationError> {
        let (controller_count, pool_count, account_count) = self
            .journal
            .active_allocation_counts_for_account(
                controller_id,
                snapshot.garm_pool_id.as_deref().unwrap_or_default(),
                account,
            )
            .map_err(|_| AllocationError::Journal)?;
        if controller_count > self.limits.max_allocations_controller as usize
            || pool_count > self.limits.max_allocations_pool as usize
            || account_count > self.limits.max_allocations_account as usize
        {
            return Err(AllocationError::Limit);
        }
        Ok(())
    }

    pub(super) fn validate_pool(&self, request: &ProviderRequest) -> Result<(), AllocationError> {
        let snapshot_id = PoolSnapshotId::resolve(request.pool_extra_specs.as_ref(), None)
            .map_err(|_| AllocationError::Snapshot)?
            .ok_or(AllocationError::Snapshot)?;
        let snapshot = self
            .journal
            .pool_snapshot_by_id(snapshot_id.as_str())
            .map_err(|_| AllocationError::Journal)?
            .ok_or(AllocationError::Snapshot)?;
        let document: SnapshotDocument =
            serde_json::from_str(&snapshot.config_json).map_err(|_| AllocationError::Snapshot)?;
        let ingress = document
            .central
            .ingress
            .as_ref()
            .ok_or(AllocationError::Snapshot)?;
        if snapshot.state != "enabled"
            || snapshot.expires_at <= unix_now()
            || self
                .journal
                .run_is_terminal(
                    document.base_repository_id,
                    document.run_id,
                    document.run_attempt,
                )
                .map_err(|_| AllocationError::Journal)?
            || ingress.controller_id != request.controller_id
            || request.pool_id.as_deref() != snapshot.garm_pool_id.as_deref()
        {
            return Err(AllocationError::Snapshot);
        }
        Ok(())
    }
}

pub(super) fn offer_requirements(
    offer: &CatalogOffer,
) -> Result<ProviderRequirements, AllocationError> {
    Ok(ProviderRequirements {
        os: offer.candidate.os.clone(),
        architecture: offer.candidate.architecture.clone(),
        purchasing: purchasing_capability(&offer.candidate.purchasing)?,
    })
}
pub(super) fn binding_capability(
    binding: &ProviderBinding,
) -> Result<(String, String, PurchasingCapability), AllocationError> {
    let os = binding
        .supported_os
        .iter()
        .next()
        .cloned()
        .ok_or(AllocationError::Configuration)?;
    let architecture = binding
        .supported_architectures
        .iter()
        .next()
        .cloned()
        .ok_or(AllocationError::Configuration)?;
    let purchasing = binding
        .purchasing_capabilities
        .iter()
        .next()
        .cloned()
        .ok_or(AllocationError::Configuration)?;
    Ok((os, architecture, purchasing))
}
pub(super) fn selection_matches_binding(
    selection: &ProviderSelection,
    binding: &ProviderBinding,
) -> bool {
    selection.binding_id == binding.id
        && selection.source_revision == binding.source_revision
        && selection.binary_sha256 == binding.executable_sha256
        && selection.configuration_sha256 == binding.configuration_sha256
}
pub(super) fn selected_image_supports_hook(config: &ConfigPatch, offer: &CatalogOffer) -> bool {
    match offer.candidate.cloud.as_str() {
        "aws" => config
            .aws_catalog_bindings
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|catalog| {
                catalog.provider_binding_id == offer.selection.binding_id
                    && catalog.images.values().any(|image| {
                        image.image_id == offer.selection.image
                            && if offer.candidate.os == "linux" {
                                image.cloud_init
                            } else {
                                image.ec2_launch_agent
                            }
                    })
            }),
        "azure" => config
            .azure_catalog_bindings
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|catalog| {
                catalog.provider_binding_id == offer.selection.binding_id
                    && catalog.images.values().any(|image| {
                        image.image_id == offer.selection.image
                            && image.capabilities.iter().any(|capability| {
                                matches!(
                                    capability.to_ascii_lowercase().as_str(),
                                    "cloud-init" | "cloud_init"
                                )
                            })
                    })
            }),
        "gcp" => config
            .gcp_catalog_bindings
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|catalog| {
                catalog.provider_binding_id == offer.selection.binding_id
                    && catalog.images.values().any(|image| {
                        image.image_id == offer.selection.image
                            && image.capabilities.iter().any(|capability| {
                                matches!(
                                    capability.to_ascii_lowercase().as_str(),
                                    "cloud-init" | "cloud_init"
                                )
                            })
                    })
            }),
        _ => false,
    }
}
pub(super) fn purchasing_capability(value: &str) -> Result<PurchasingCapability, AllocationError> {
    match value {
        "on_demand" => Ok(PurchasingCapability::OnDemand),
        "spot" => Ok(PurchasingCapability::Spot),
        _ => Err(AllocationError::NoCandidate),
    }
}
