use super::catalog::catalog_configuration_fingerprint;
use super::catalog::configured_catalog_fingerprints;
use super::history::ExplanationHistoryContext;
use super::{
    AllocationError, AllocationService, CatalogOffer, ConfigPatch, Duration, EffectivePrice,
    EffectivePriceComponent, ExplainRequest, ExplanationCandidate,
    ExplanationEffectivePriceOverride, ExplanationGcpLicense, ExplanationHistory,
    ExplanationLaunchFacts, ExplanationPolicyAttempt, ExplanationPolicyCandidate, ExplanationPrice,
    ExplanationPriceComponent, ExplanationReport, ExplanationRequirements, ExplanationSnapshot,
    ExplanationTechnicalEvidence, FinancialPriceScope, HashMap, Instant, ProviderLaunchFacts,
    SnapshotDocument, unix_now,
};

impl ExplanationReport {
    pub(super) fn trim_to_size(&mut self) -> Result<(), AllocationError> {
        while serde_json::to_vec(self)
            .map_err(|_| AllocationError::Configuration)?
            .len()
            > 1_000_000
        {
            if let Some(attempt) = self
                .policy_attempts
                .last_mut()
                .filter(|attempt| !attempt.candidates.is_empty())
            {
                attempt.candidates.pop();
                attempt.candidates_truncated = true;
            } else if self.policy_attempts.len() > 1 {
                self.policy_attempts.remove(0);
                self.policy_attempts_truncated = true;
            } else if !self.observed_exclusions.is_empty() {
                self.observed_exclusions.pop();
                self.observed_exclusions_truncated = true;
            } else if !self.queried_catalogs.is_empty() {
                self.queried_catalogs.pop();
                self.queried_catalogs_truncated = true;
            } else if self.candidates.len() > 1 {
                self.candidates.pop();
                self.candidates_truncated = true;
            } else {
                return Err(AllocationError::Configuration);
            }
        }
        Ok(())
    }
}

pub(super) fn explanation_candidate(
    offer: &CatalogOffer,
    explanation: Option<&crate::policy::CandidateExplanation>,
    policy_error: Option<&str>,
    central: &ConfigPatch,
    analyzed_at: u64,
    fingerprint: &str,
    history_context: ExplanationHistoryContext<'_>,
) -> Result<ExplanationCandidate, AllocationError> {
    let candidate = &offer.candidate;
    let (financial_price_scope, technical_evidence) =
        explanation_financial_scope(offer, central, fingerprint)?;
    let price = candidate
        .price
        .as_ref()
        .map(|price| explanation_price(price, analyzed_at));
    Ok(ExplanationCandidate {
        identity: bounded_text(&candidate.identity, 512),
        text_truncated: candidate.identity.chars().count() > 512
            || explanation.map_or_else(
                || {
                    policy_error
                        .unwrap_or("candidate was not ranked")
                        .chars()
                        .count()
                        > 256
                },
                |item| item.reason.chars().count() > 256,
            ),
        eligible: explanation.is_some_and(|item| item.eligible),
        reason: explanation.map_or_else(
            || bounded_text(policy_error.unwrap_or("candidate was not ranked"), 256),
            |item| bounded_text(&item.reason, 256),
        ),
        score: explanation.and_then(|item| item.score),
        secondary_score: explanation.and_then(|item| item.secondary_score),
        purchasing_tier: explanation.and_then(|item| item.purchasing_tier),
        price_age_seconds: candidate
            .price
            .as_ref()
            .and_then(|quote| quote.observed_age_seconds),
        score_terms: explanation.map_or_else(Vec::new, |item| item.score_terms.clone()),
        capabilities: candidate
            .capabilities
            .iter()
            .take(64)
            .map(|value| bounded_text(value, 128))
            .collect(),
        capability_count: candidate.capabilities.len(),
        capabilities_truncated: candidate.capabilities.len() > 64,
        technical_evidence_count: technical_evidence.len(),
        technical_evidence_truncated: technical_evidence.len() > 16,
        technical_evidence: technical_evidence.into_iter().take(16).collect(),
        financial_price_scope,
        price,
        history: ExplanationHistory {
            sample_count: candidate.observation_samples,
            create_outcomes: candidate.definitive_create_outcomes,
            latency_samples: candidate.measured_latency_samples,
            interruption_samples: candidate.interruption_samples,
            mean_create_seconds: candidate.mean_create_seconds,
            failure_rate: candidate.allocation_failure_rate,
            interruptions_per_hour: candidate.interruption_rate_per_hour,
            observation_ages: candidate.observation_ages.clone(),
            observation_window: candidate.observation_window.clone(),
            queried_window: history_context.query_window.cloned(),
            unknown_reasons: history_context.unknown_reasons.to_vec(),
        },
    })
}
pub(super) fn explanation_financial_scope(
    offer: &CatalogOffer,
    central: &ConfigPatch,
    fingerprint: &str,
) -> Result<(FinancialPriceScope, Vec<ExplanationTechnicalEvidence>), AllocationError> {
    let candidate = &offer.candidate;
    let (native_account, native_location, launch_facts, technical_evidence) =
        match &offer.launch_facts {
            ProviderLaunchFacts::Aws(facts) => {
                let binding = central
                    .aws_catalog_bindings
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .find(|binding| {
                        binding.provider_binding_id == offer.selection.binding_id
                            && catalog_configuration_fingerprint(central, "aws", &binding.id)
                                .is_ok_and(|value| value == fingerprint)
                    })
                    .ok_or(AllocationError::Configuration)?;
                if binding.account_id.trim().is_empty() || binding.region.trim().is_empty() {
                    return Err(AllocationError::Configuration);
                }
                let location = binding
                    .locations
                    .get(&candidate.location)
                    .ok_or(AllocationError::Configuration)?;
                let image = binding
                    .images
                    .get(&candidate.image)
                    .ok_or(AllocationError::Configuration)?;
                if image.image_id != offer.selection.image {
                    return Err(AllocationError::Configuration);
                }
                (
                    binding.account_id.clone(),
                    binding.region.clone(),
                    ExplanationLaunchFacts::Aws {
                        availability_zone: facts.availability_zone.clone(),
                        subnet_id: facts.subnet_id.clone(),
                        security_group_ids: location.security_group_ids.clone(),
                        map_public_ip_on_launch: facts.map_public_ip_on_launch,
                        resolved_image_id: image.image_id.clone(),
                        windows_license_included: image.windows_license_included,
                        root_device_name: facts.root_device_name.clone(),
                        minimum_root_volume_gib: facts.root_volume_minimum_gib,
                    },
                    Vec::new(),
                )
            }
            ProviderLaunchFacts::Azure(facts) => {
                let binding = central
                    .azure_catalog_bindings
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .find(|binding| {
                        binding.provider_binding_id == offer.selection.binding_id
                            && catalog_configuration_fingerprint(central, "azure", &binding.id)
                                .is_ok_and(|value| value == fingerprint)
                    })
                    .ok_or(AllocationError::Configuration)?;
                if binding.subscription_id.trim().is_empty() {
                    return Err(AllocationError::Configuration);
                }
                let location = binding
                    .locations
                    .get(&candidate.location)
                    .ok_or(AllocationError::Configuration)?;
                if location.location.trim().is_empty() {
                    return Err(AllocationError::Configuration);
                }
                let image = binding
                    .images
                    .get(&candidate.image)
                    .ok_or(AllocationError::Configuration)?;
                if image.image_id != offer.selection.image {
                    return Err(AllocationError::Configuration);
                }
                (
                    binding.subscription_id.clone(),
                    location.location.clone(),
                    ExplanationLaunchFacts::Azure {
                        subscription_id: facts.subscription_id.clone(),
                        native_location: facts.location.clone(),
                        resolved_image_version: facts.image_version.clone(),
                        os_disk_sku: facts.os_disk_sku.clone(),
                        public_ip_sku: facts.public_ip_sku.clone(),
                        vnet_subnet_id: location.vnet_subnet_id.clone(),
                        minimum_os_disk_gib: facts.minimum_os_disk_gib,
                        image_licensing: facts.image_licensing,
                    },
                    vec![
                        ExplanationTechnicalEvidence {
                            name: "minimum_os_disk_source".to_owned(),
                            value: bounded_text(&facts.minimum_os_disk_source, 256),
                            value_truncated: facts.minimum_os_disk_source.chars().count() > 256,
                        },
                        ExplanationTechnicalEvidence {
                            name: "image_licensing_source".to_owned(),
                            value: bounded_text(&facts.image_licensing_source, 256),
                            value_truncated: facts.image_licensing_source.chars().count() > 256,
                        },
                    ],
                )
            }
            ProviderLaunchFacts::Gcp(facts) => {
                let binding = central
                    .gcp_catalog_bindings
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .find(|binding| {
                        binding.provider_binding_id == offer.selection.binding_id
                            && catalog_configuration_fingerprint(central, "gcp", &binding.id)
                                .is_ok_and(|value| value == fingerprint)
                    })
                    .ok_or(AllocationError::Configuration)?;
                if binding.project_id.trim().is_empty() || binding.zone.trim().is_empty() {
                    return Err(AllocationError::Configuration);
                }
                let image = binding
                    .images
                    .get(&candidate.image)
                    .ok_or(AllocationError::Configuration)?;
                if image.image_id != facts.image_id || image.image_id != offer.selection.image {
                    return Err(AllocationError::Configuration);
                }
                (
                    binding.project_id.clone(),
                    binding.zone.clone(),
                    ExplanationLaunchFacts::Gcp {
                        project_id: facts.project_id.clone(),
                        native_zone: facts.zone.clone(),
                        network_id: facts.network_id.clone(),
                        subnetwork_id: facts.subnetwork_id.clone(),
                        external_ip_access: facts.external_ip_access,
                        resolved_image_id: facts.image_id.clone(),
                        image_minimum_disk_gib: facts.image_minimum_disk_gib,
                        bundled_local_ssd_partitions: facts.bundled_local_ssd_partitions,
                        licenses: image
                            .license_facts
                            .iter()
                            .map(|(license_uri, license)| ExplanationGcpLicense {
                                license_uri: license_uri.clone(),
                                basis: license.basis,
                                billing_sku: license.billing_sku.clone(),
                            })
                            .collect(),
                    },
                    vec![ExplanationTechnicalEvidence {
                        name: "license_free_source".to_owned(),
                        value: image
                            .license_free_source
                            .as_deref()
                            .map(|value| bounded_text(value, 256))
                            .unwrap_or_default(),
                        value_truncated: image
                            .license_free_source
                            .as_deref()
                            .is_some_and(|value| value.chars().count() > 256),
                    }],
                )
            }
        };
    let financial_price_scope = FinancialPriceScope {
        cloud: candidate.cloud.clone(),
        configured_account: candidate.account.clone(),
        native_account,
        configured_location: candidate.location.clone(),
        native_location,
        binding_id: offer.selection.binding_id.clone(),
        machine_type: candidate.machine_type.clone(),
        os: candidate.os.clone(),
        architecture: candidate.architecture.clone(),
        image_alias: candidate.image.clone(),
        resolved_image: match &offer.launch_facts {
            ProviderLaunchFacts::Aws(_) => offer.selection.image.clone(),
            ProviderLaunchFacts::Azure(facts) => facts.image_version.clone(),
            ProviderLaunchFacts::Gcp(facts) => facts.image_id.clone(),
        },
        purchasing: candidate.purchasing.clone(),
        vcpu: candidate.vcpu,
        memory_gib: candidate.memory_gib,
        gpu_vendor: candidate.gpu_vendor.clone(),
        gpu_model: candidate.gpu_model.clone(),
        gpu_count: candidate.gpu_count,
        gpu_memory_gib: candidate.gpu_memory_gib,
        boot_disk_class: candidate.boot_disk_class.clone(),
        boot_disk_gib: candidate.boot_disk_gib,
        boot_disk_iops: candidate.boot_disk_iops,
        boot_disk_throughput_mib_s: candidate.boot_disk_throughput_mib_s,
        local_scratch_gib: candidate.local_scratch_gib,
        launch_facts,
    };
    Ok((financial_price_scope, technical_evidence))
}
pub(super) fn apply_effective_prices(
    quote: &mut crate::policy::PriceQuote,
    scope: &FinancialPriceScope,
    effective_prices: &[EffectivePrice],
    analyzed_at: u64,
) -> Result<(), AllocationError> {
    for price in effective_prices {
        if price.scope != *scope
            || analyzed_at < price.valid_from_unix_seconds
            || analyzed_at >= price.valid_until_unix_seconds
        {
            continue;
        }
        if quote.currency != price.currency {
            continue;
        }
        let component = match price.component {
            EffectivePriceComponent::Compute => &mut quote.compute,
            EffectivePriceComponent::Disk => &mut quote.disk,
            EffectivePriceComponent::Licensing => &mut quote.licensing,
            EffectivePriceComponent::PublicAddress => &mut quote.public_address,
        };
        if component.included_in_compute {
            return Err(AllocationError::Configuration);
        }
        component.effective_price_override = Some(crate::policy::AppliedEffectivePrice {
            id: price.id.clone(),
            reference: price.reference.clone(),
            valid_from_unix_seconds: price.valid_from_unix_seconds,
            valid_until_unix_seconds: price.valid_until_unix_seconds,
            original_hourly_rate: component.hourly_rate,
            original_source: component.source.clone(),
            original_observed_at_unix_seconds: component.observed_at_unix_seconds,
            original_effective_at_unix_seconds: component.effective_at_unix_seconds,
        });
        component.hourly_rate = Some(price.hourly_rate);
        component.source = format!("administrator effective price: {}", price.source);
        component.observed_at_unix_seconds = Some(price.observed_at_unix_seconds);
        component.effective_at_unix_seconds = Some(price.valid_from_unix_seconds);
    }
    Ok(())
}
pub(super) fn explanation_price(
    price: &crate::policy::PriceQuote,
    analyzed_at: u64,
) -> ExplanationPrice {
    ExplanationPrice {
        currency: price.currency.clone(),
        compute: explanation_price_component(&price.compute, analyzed_at),
        disk: explanation_price_component(&price.disk, analyzed_at),
        licensing: explanation_price_component(&price.licensing, analyzed_at),
        public_address: explanation_price_component(&price.public_address, analyzed_at),
        new_host_minimum_commitment_seconds: price.new_host_minimum_commitment_seconds,
        existing_host_remaining_commitment_seconds: price
            .existing_host_remaining_commitment_seconds,
        included_charge_names: price
            .included_charge_names
            .iter()
            .take(32)
            .cloned()
            .collect(),
        included_charge_count: price.included_charge_names.len(),
        included_charge_names_truncated: price.included_charge_names.len() > 32,
        exclusions: price
            .exclusions
            .iter()
            .take(32)
            .map(|value| bounded_text(value, 256))
            .collect(),
        exclusion_count: price.exclusions.len(),
        exclusions_truncated: price.exclusions.len() > 32,
    }
}
pub(super) fn explanation_policy_attempt(
    attempt: &crate::policy::PolicyAttempt,
) -> ExplanationPolicyAttempt {
    ExplanationPolicyAttempt {
        name: bounded_text(&attempt.name, 128),
        text_truncated: attempt.name.chars().count() > 128
            || attempt
                .reason
                .as_deref()
                .is_some_and(|reason| reason.chars().count() > 256)
            || attempt.candidates.iter().any(|candidate| {
                candidate.identity.chars().count() > 512 || candidate.reason.chars().count() > 256
            }),
        outcome: attempt.outcome.clone(),
        reason: attempt
            .reason
            .as_deref()
            .map(|reason| bounded_text(reason, 256)),
        candidate_count: attempt.candidates.len(),
        candidates_truncated: attempt.candidates.len() > 100,
        candidates: attempt
            .candidates
            .iter()
            .take(100)
            .map(|candidate| ExplanationPolicyCandidate {
                identity: bounded_text(&candidate.identity, 512),
                text_truncated: candidate.identity.chars().count() > 512
                    || candidate.reason.chars().count() > 256,
                eligible: candidate.eligible,
                reason: bounded_text(&candidate.reason, 256),
                score: candidate.score,
                secondary_score: candidate.secondary_score,
                purchasing_tier: candidate.purchasing_tier,
                price_age_seconds: candidate.price_age_seconds,
                score_terms: candidate.score_terms.clone(),
            })
            .collect(),
    }
}
pub(super) fn explanation_price_component(
    component: &crate::policy::PriceComponent,
    analyzed_at: u64,
) -> ExplanationPriceComponent {
    ExplanationPriceComponent {
        hourly_rate: component.hourly_rate,
        minimum_commitment_seconds: component.minimum_commitment_seconds,
        source: bounded_text(&component.source, 128),
        source_truncated: component.source.chars().count() > 128,
        observed_at_unix_seconds: component.observed_at_unix_seconds,
        effective_at_unix_seconds: component.effective_at_unix_seconds,
        age_seconds: component
            .observed_at_unix_seconds
            .and_then(|observed| analyzed_at.checked_sub(observed)),
        included_in_compute: component.included_in_compute,
        exclusions: component
            .exclusions
            .iter()
            .take(16)
            .map(|value| bounded_text(value, 256))
            .collect(),
        exclusion_count: component.exclusions.len(),
        exclusions_truncated: component.exclusions.len() > 16,
        effective_price_override: component.effective_price_override.as_ref().map(
            |override_record| ExplanationEffectivePriceOverride {
                id: override_record.id.clone(),
                reference: override_record.reference.clone(),
                valid_from_unix_seconds: override_record.valid_from_unix_seconds,
                valid_until_unix_seconds: override_record.valid_until_unix_seconds,
                original_hourly_rate: override_record.original_hourly_rate,
                original_source: bounded_text(&override_record.original_source, 256),
                original_source_truncated: override_record.original_source.chars().count() > 256,
                original_observed_at_unix_seconds: override_record
                    .original_observed_at_unix_seconds,
                original_effective_at_unix_seconds: override_record
                    .original_effective_at_unix_seconds,
            },
        ),
    }
}
pub(super) fn bounded_text(value: &str, maximum_chars: usize) -> String {
    value.chars().take(maximum_chars).collect()
}
impl AllocationService {
    /// Explain the same read-only analysis used by Create.
    ///
    /// # Errors
    /// Returns an error when the profile/snapshot is invalid, catalog analysis
    /// fails, or a bounded report cannot be constructed.
    pub async fn explain(
        &self,
        request: ExplainRequest,
    ) -> Result<ExplanationReport, AllocationError> {
        if request.profile.is_some() && request.snapshot_id.is_some() {
            return Err(AllocationError::Snapshot);
        }
        let (mut profile, central, fingerprints, snapshot_report, input_source) =
            if let Some(snapshot_id) = request.snapshot_id {
                let snapshot = self
                    .journal
                    .pool_snapshot_by_id(&snapshot_id)
                    .map_err(|_| AllocationError::Journal)?
                    .ok_or(AllocationError::Snapshot)?;
                let document: SnapshotDocument = serde_json::from_str(&snapshot.config_json)
                    .map_err(|_| AllocationError::Snapshot)?;
                let ingress = document
                    .central
                    .ingress
                    .as_ref()
                    .ok_or(AllocationError::Snapshot)?;
                if snapshot.state != "enabled"
                    || snapshot.expires_at <= unix_now()
                    || snapshot.garm_pool_id.is_none()
                    || self
                        .journal
                        .run_is_terminal(
                            document.base_repository_id,
                            document.run_id,
                            document.run_attempt,
                        )
                        .map_err(|_| AllocationError::Journal)?
                    || self
                        .config
                        .ingress
                        .as_ref()
                        .is_none_or(|current| current.controller_id != ingress.controller_id)
                {
                    return Err(AllocationError::Snapshot);
                }
                let fingerprints = configured_catalog_fingerprints(&document.central)?;
                let report = ExplanationSnapshot {
                    id: snapshot.id,
                    repository_id: document.base_repository_id,
                    run_id: document.run_id,
                    run_attempt: document.run_attempt,
                    job_id: document.job_id,
                    commit_sha: document.commit_sha,
                };
                (
                    document.resolved_profile,
                    document.central,
                    fingerprints,
                    Some(report),
                    "retained_repository_job_snapshot".to_owned(),
                )
            } else {
                let name = request.profile.as_deref().unwrap_or("default");
                let normalized = self
                    .config
                    .normalize_profile(None, name, None)
                    .map_err(|_| AllocationError::Configuration)?;
                let fingerprints = configured_catalog_fingerprints(&self.config)?;
                (
                    normalized.profile,
                    self.config.clone(),
                    fingerprints,
                    None,
                    "current_central_profile".to_owned(),
                )
            };
        profile.administrator_limits = self.limits.clone();
        let ingress = central
            .ingress
            .as_ref()
            .ok_or(AllocationError::Configuration)?;
        let bootstrap_seconds = u64::from(ingress.bootstrap_timeout_minutes)
            .checked_mul(60)
            .ok_or(AllocationError::Configuration)?;
        if ingress.allocation_timeout_seconds <= ingress.timeout_margin_seconds
            || bootstrap_seconds <= ingress.timeout_margin_seconds
        {
            return Err(AllocationError::Configuration);
        }
        let budget = ingress
            .allocation_timeout_seconds
            .min(bootstrap_seconds)
            .saturating_sub(ingress.timeout_margin_seconds);
        let deadline = Instant::now() + Duration::from_secs(budget);
        let analysis = self
            .rank_offers(&profile, &central, &fingerprints, deadline)
            .await?;
        let now = analysis.analyzed_at_unix_seconds;
        let explanations = analysis
            .ranking
            .as_ref()
            .map(|ranking| ranking.candidates.as_slice())
            .or_else(|| {
                analysis
                    .policy_attempts
                    .last()
                    .map(|attempt| attempt.candidates.as_slice())
            })
            .unwrap_or_default();
        let explanations_by_identity = explanations
            .iter()
            .map(|item| (item.identity.as_str(), item))
            .collect::<HashMap<_, _>>();
        let mut candidates = Vec::new();
        for (offer, fingerprint) in analysis.offers.iter().take(100) {
            let candidate = &offer.candidate;
            let scored = explanations_by_identity
                .get(candidate.identity.as_str())
                .copied();
            let history_unknown_reasons = analysis
                .history_unknown_reasons
                .get(&candidate.identity)
                .cloned()
                .unwrap_or_else(|| {
                    if central.history.is_none() {
                        vec!["history collection is not configured".to_owned()]
                    } else {
                        Vec::new()
                    }
                });
            candidates.push(explanation_candidate(
                offer,
                scored,
                analysis.policy_error.as_deref(),
                &central,
                now,
                fingerprint,
                ExplanationHistoryContext {
                    unknown_reasons: &history_unknown_reasons,
                    query_window: analysis.history_query_window.as_ref(),
                },
            )?);
        }
        let candidate_count = analysis.offers.len();
        let candidates_truncated = candidate_count > candidates.len();
        let queried_catalog_count = analysis.queried_catalogs.len();
        let observed_exclusion_count = analysis.observed_exclusions.len();
        let policy_attempt_count = analysis.policy_attempts.len();
        let mut policy_attempts = analysis
            .policy_attempts
            .iter()
            .take(32)
            .map(explanation_policy_attempt)
            .collect::<Vec<_>>();
        if let Some(final_attempt) = analysis.policy_attempts.last()
            && policy_attempt_count > 32
            && !policy_attempts
                .iter()
                .any(|attempt| attempt.name == final_attempt.name)
        {
            policy_attempts.pop();
            policy_attempts.push(explanation_policy_attempt(final_attempt));
        }
        let queried_catalogs = analysis.queried_catalogs.iter().take(64).cloned().collect();
        let maximum_total_cost = match (
            profile.maximum_total_cost,
            profile.administrator_limits.max_total_cost,
        ) {
            (Some(user), Some(administrator)) => Some(user.min(administrator)),
            (Some(user), None) => Some(user),
            (None, Some(administrator)) => Some(administrator),
            (None, None) => None,
        };
        let mut report = ExplanationReport {
            input_source,
            snapshot: snapshot_report,
            analyzed_at_unix_seconds: now,
            cost_horizon_seconds: profile.cost_horizon_seconds,
            normalized_requirements: ExplanationRequirements {
                os: profile.os.clone(),
                architecture: profile.architecture.clone(),
                vcpu_min: profile.vcpu_min,
                vcpu_max: profile.vcpu_max,
                memory_gib_min: rust_decimal::Decimal::from(profile.memory_gib_min),
                memory_gib_max: rust_decimal::Decimal::from(profile.memory_gib_max),
                purchasing_tiers: profile.purchasing_tiers.clone(),
                machine_types: profile.machine_types.clone(),
                machine_families: profile.machine_families.clone(),
                accounts: profile.accounts.clone(),
                locations: profile.locations.clone(),
                gpu: profile.gpu.clone(),
                boot_disk: profile.boot_disk.clone(),
                local_scratch_gib: profile.local_scratch_gib,
                capabilities: profile.capabilities.iter().cloned().collect(),
                purchasing_models: profile.purchasing_models.iter().cloned().collect(),
                maximum_total_cost,
                administrator_max_vcpu: profile.administrator_limits.max_vcpu,
                administrator_max_memory_gib: profile.administrator_limits.max_memory_gib,
                administrator_max_boot_disk_gib: profile.administrator_limits.max_boot_disk_gib,
                administrator_max_boot_disk_iops: profile.administrator_limits.max_boot_disk_iops,
                administrator_max_boot_disk_throughput_mib_s: profile
                    .administrator_limits
                    .max_boot_disk_throughput_mib_s,
                administrator_max_local_scratch_gib: profile
                    .administrator_limits
                    .max_local_scratch_gib,
                administrator_max_gpu_count: profile.administrator_limits.max_gpu_count,
                administrator_max_gpu_memory_gib: profile.administrator_limits.max_gpu_memory_gib,
                max_price_age_seconds: profile.max_price_age_seconds,
                policy: profile.policy.clone(),
                image: profile.image.clone(),
                cost_horizon_seconds: profile.cost_horizon_seconds,
            },
            queried_catalogs,
            queried_catalog_count,
            queried_catalogs_truncated: queried_catalog_count > 64,
            observed_exclusion_count,
            observed_exclusions_truncated: observed_exclusion_count > 64,
            observed_exclusions_exhaustive: false,
            observed_exclusions: analysis
                .observed_exclusions
                .iter()
                .take(64)
                .cloned()
                .collect(),
            policy_attempt_count,
            policy_attempts_truncated: policy_attempt_count > policy_attempts.len(),
            applied_policy: analysis
                .ranking
                .as_ref()
                .map(|ranking| ranking.applied_policy.clone()),
            fallback_from: analysis
                .ranking
                .as_ref()
                .and_then(|ranking| ranking.fallback_from.clone()),
            policy_attempts,
            policy_error: analysis.policy_error,
            candidates,
            candidate_count,
            candidates_truncated,
            quote_missing: analysis.quote_missing,
        };
        report.trim_to_size()?;
        Ok(report)
    }
}
