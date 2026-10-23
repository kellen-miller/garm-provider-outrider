use super::explain::apply_effective_prices;
use super::{
    BTreeMap, BTreeSet, EffectivePrice, EffectivePriceComponent, ExplanationCandidate,
    ExplanationHistory, ExplanationLaunchFacts, ExplanationPolicyAttempt,
    ExplanationPolicyCandidate, ExplanationReport, ExplanationRequirements, FinancialPriceScope,
    SystemTime, UNIX_EPOCH,
};

fn financial_scope() -> FinancialPriceScope {
    FinancialPriceScope {
        cloud: "aws".to_owned(),
        configured_account: "account".to_owned(),
        native_account: "123456789012".to_owned(),
        configured_location: "east-ci".to_owned(),
        native_location: "us-east-1".to_owned(),
        binding_id: "aws-east".to_owned(),
        machine_type: "c7i.large".to_owned(),
        os: "linux".to_owned(),
        architecture: "amd64".to_owned(),
        image_alias: "ubuntu-24".to_owned(),
        resolved_image: "ami-0123456789abcdef0".to_owned(),
        purchasing: "on_demand".to_owned(),
        vcpu: 2,
        memory_gib: rust_decimal::Decimal::from(4),
        gpu_vendor: None,
        gpu_model: None,
        gpu_count: 0,
        gpu_memory_gib: None,
        boot_disk_class: "gp3".to_owned(),
        boot_disk_gib: 30,
        boot_disk_iops: 3000,
        boot_disk_throughput_mib_s: rust_decimal::Decimal::from(125),
        local_scratch_gib: rust_decimal::Decimal::ZERO,
        launch_facts: ExplanationLaunchFacts::Aws {
            availability_zone: "us-east-1a".to_owned(),
            subnet_id: "subnet-123".to_owned(),
            security_group_ids: Vec::new(),
            map_public_ip_on_launch: false,
            resolved_image_id: "ami-0123456789abcdef0".to_owned(),
            windows_license_included: false,
            root_device_name: "/dev/sda1".to_owned(),
            minimum_root_volume_gib: 30,
        },
    }
}

fn effective_price(observed_at: u64, valid_from: u64, valid_until: u64) -> EffectivePrice {
    EffectivePrice {
        id: "negotiated-compute".to_owned(),
        scope: financial_scope(),
        component: EffectivePriceComponent::Compute,
        currency: "USD".to_owned(),
        hourly_rate: rust_decimal::Decimal::new(42, 2),
        source: "vendor agreement".to_owned(),
        reference: "agreement-section-4".to_owned(),
        observed_at_unix_seconds: observed_at,
        valid_from_unix_seconds: valid_from,
        valid_until_unix_seconds: valid_until,
    }
}

fn quote() -> crate::policy::PriceQuote {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let public_component = crate::policy::PriceComponent {
        hourly_rate: Some(rust_decimal::Decimal::new(84, 2)),
        minimum_commitment_seconds: 900,
        source: "public rate sheet".to_owned(),
        observed_at_unix_seconds: Some(now.saturating_sub(50)),
        effective_at_unix_seconds: Some(now.saturating_sub(100)),
        included_in_compute: false,
        exclusions: vec!["tax excluded".to_owned()],
        effective_price_override: None,
    };
    crate::policy::PriceQuote {
        currency: "USD".to_owned(),
        provider_request_ids: vec!["request-123".to_owned()],
        compute: crate::policy::PriceComponent {
            hourly_rate: None,
            ..public_component.clone()
        },
        disk: public_component.clone(),
        licensing: public_component.clone(),
        public_address: public_component,
        new_host_minimum_commitment_seconds: Some(3600),
        existing_host_remaining_commitment_seconds: Some(1800),
        observed_age_seconds: None,
        included_charge_names: BTreeSet::from(["host".to_owned()]),
        exclusions: vec!["provider exclusion".to_owned()],
    }
}

fn assert_scope_mismatch(scope: FinancialPriceScope, price: &EffectivePrice, now: u64) {
    let mut quote = quote();
    apply_effective_prices(&mut quote, &scope, std::slice::from_ref(price), now).unwrap();
    assert_eq!(quote.compute.hourly_rate, None);
}

#[test]
fn byte_trimming_keeps_winner_and_truthful_candidate_totals() {
    let mut report = ExplanationReport {
        input_source: "current_central_profile".to_owned(),
        snapshot: None,
        analyzed_at_unix_seconds: 1,
        cost_horizon_seconds: 3600,
        normalized_requirements: ExplanationRequirements {
            os: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            vcpu_min: 1,
            vcpu_max: 1,
            memory_gib_min: rust_decimal::Decimal::ONE,
            memory_gib_max: rust_decimal::Decimal::ONE,
            purchasing_tiers: Vec::new(),
            machine_types: BTreeMap::new(),
            machine_families: BTreeMap::new(),
            accounts: Vec::new(),
            locations: Vec::new(),
            gpu: None,
            boot_disk: crate::config::DiskRequirement {
                class: "gp3".to_owned(),
                size_gib: 8,
                iops: None,
                throughput_mib_s: None,
            },
            local_scratch_gib: 0,
            capabilities: Vec::new(),
            purchasing_models: Vec::new(),
            maximum_total_cost: None,
            administrator_max_vcpu: 1,
            administrator_max_memory_gib: 1,
            administrator_max_boot_disk_gib: 8,
            administrator_max_boot_disk_iops: 0,
            administrator_max_boot_disk_throughput_mib_s: 0,
            administrator_max_local_scratch_gib: 0,
            administrator_max_gpu_count: 0,
            administrator_max_gpu_memory_gib: 0,
            max_price_age_seconds: 60,
            policy: "economy".to_owned(),
            image: "linux".to_owned(),
            cost_horizon_seconds: 3600,
        },
        queried_catalogs: Vec::new(),
        queried_catalog_count: 0,
        queried_catalogs_truncated: false,
        observed_exclusions: Vec::new(),
        observed_exclusion_count: 0,
        observed_exclusions_truncated: false,
        observed_exclusions_exhaustive: false,
        policy_attempts: vec![ExplanationPolicyAttempt {
            name: "economy".to_owned(),
            text_truncated: false,
            outcome: "applied".to_owned(),
            reason: None,
            candidates: (0..50)
                .map(|index| ExplanationPolicyCandidate {
                    identity: format!("offer-{index}"),
                    text_truncated: false,
                    eligible: false,
                    reason: "x".repeat(25_000),
                    score: None,
                    secondary_score: None,
                    purchasing_tier: None,
                    price_age_seconds: None,
                    score_terms: Vec::new(),
                })
                .collect(),
            candidate_count: 50,
            candidates_truncated: false,
        }],
        policy_attempt_count: 1,
        policy_attempts_truncated: false,
        applied_policy: Some("economy".to_owned()),
        fallback_from: None,
        policy_error: None,
        candidates: vec![ExplanationCandidate {
            identity: "winner".to_owned(),
            text_truncated: false,
            eligible: true,
            reason: "lowest eligible cost".to_owned(),
            score: Some(rust_decimal::Decimal::ONE),
            secondary_score: None,
            purchasing_tier: Some(0),
            price_age_seconds: Some(1),
            score_terms: Vec::new(),
            capabilities: Vec::new(),
            capability_count: 0,
            capabilities_truncated: false,
            financial_price_scope: FinancialPriceScope {
                cloud: "aws".to_owned(),
                configured_account: "account".to_owned(),
                native_account: "123456789012".to_owned(),
                configured_location: "use1".to_owned(),
                native_location: "us-east-1".to_owned(),
                binding_id: "binding".to_owned(),
                machine_type: "m7i.large".to_owned(),
                os: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                image_alias: "linux".to_owned(),
                resolved_image: "ami-123".to_owned(),
                purchasing: "on_demand".to_owned(),
                vcpu: 1,
                memory_gib: rust_decimal::Decimal::ONE,
                gpu_vendor: None,
                gpu_model: None,
                gpu_count: 0,
                gpu_memory_gib: None,
                boot_disk_class: "gp3".to_owned(),
                boot_disk_gib: 8,
                boot_disk_iops: 0,
                boot_disk_throughput_mib_s: rust_decimal::Decimal::ZERO,
                local_scratch_gib: rust_decimal::Decimal::ZERO,
                launch_facts: ExplanationLaunchFacts::Aws {
                    availability_zone: "us-east-1a".to_owned(),
                    subnet_id: "subnet-123".to_owned(),
                    security_group_ids: vec!["sg-123".to_owned()],
                    map_public_ip_on_launch: false,
                    resolved_image_id: "ami-123".to_owned(),
                    windows_license_included: false,
                    root_device_name: "/dev/sda1".to_owned(),
                    minimum_root_volume_gib: 8,
                },
            },
            technical_evidence: Vec::new(),
            technical_evidence_count: 0,
            technical_evidence_truncated: false,
            price: None,
            history: ExplanationHistory {
                sample_count: 0,
                create_outcomes: 0,
                latency_samples: 0,
                interruption_samples: 0,
                mean_create_seconds: None,
                failure_rate: None,
                interruptions_per_hour: None,
                observation_ages: None,
                observation_window: None,
                queried_window: None,
                unknown_reasons: Vec::new(),
            },
        }],
        candidate_count: 1,
        candidates_truncated: false,
        quote_missing: false,
    };

    report.trim_to_size().unwrap();

    let encoded = serde_json::to_vec(&report).unwrap();
    assert!(encoded.len() <= 1_000_000);
    assert_eq!(report.candidate_count, 1);
    assert_eq!(report.candidates[0].identity, "winner");
    assert!(report.candidates[0].eligible);
    assert_eq!(report.applied_policy.as_deref(), Some("economy"));
    assert_eq!(report.policy_attempts[0].candidate_count, 50);
    assert!(report.policy_attempts[0].candidates_truncated);
}

#[test]
fn effective_price_fills_only_one_component_and_preserves_public_provenance() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut quote = quote();
    let price = effective_price(now - 50, now - 100, now + 100);
    apply_effective_prices(&mut quote, &financial_scope(), &[price], now).unwrap();
    quote.refresh_observed_age();
    let mut new_host_quote = quote.clone();
    new_host_quote.existing_host_remaining_commitment_seconds = None;

    assert_eq!(
        quote.compute.hourly_rate,
        Some(rust_decimal::Decimal::new(42, 2))
    );
    assert_eq!(
        quote.disk.hourly_rate,
        Some(rust_decimal::Decimal::new(84, 2))
    );
    assert_eq!(quote.compute.minimum_commitment_seconds, 900);
    assert_eq!(quote.new_host_minimum_commitment_seconds, Some(3600));
    assert_eq!(quote.existing_host_remaining_commitment_seconds, Some(1800));
    assert_eq!(
        new_host_quote.estimated_total(60).unwrap(),
        rust_decimal::Decimal::new(105, 2)
    );
    assert_eq!(quote.provider_request_ids, ["request-123"]);
    assert_eq!(quote.exclusions, ["provider exclusion"]);
    assert_eq!(quote.compute.exclusions, ["tax excluded"]);
    let provenance = quote.compute.effective_price_override.as_ref().unwrap();
    assert_eq!(provenance.id, "negotiated-compute");
    assert_eq!(provenance.original_hourly_rate, None);
    assert_eq!(provenance.original_source, "public rate sheet");
    assert!(
        provenance
            .original_observed_at_unix_seconds
            .is_some_and(|observed| observed <= now && observed >= now.saturating_sub(60))
    );
    assert!(
        provenance
            .original_effective_at_unix_seconds
            .is_some_and(|effective| effective <= now && effective >= now.saturating_sub(110))
    );
    assert!(quote.observed_age_seconds.is_some_and(|age| age >= 50));
}

#[test]
fn effective_price_does_not_hide_a_second_missing_component() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut quote = quote();
    quote.licensing.hourly_rate = None;
    let price = effective_price(now - 1, now - 10, now + 10);

    apply_effective_prices(&mut quote, &financial_scope(), &[price], now).unwrap();

    assert_eq!(
        quote.compute.hourly_rate,
        Some(rust_decimal::Decimal::new(42, 2))
    );
    assert_eq!(quote.licensing.hourly_rate, None);
    assert!(quote.estimated_total(3600).is_err());
}

#[test]
fn effective_price_boundaries_future_and_stale_evidence_remain_truthful() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut not_yet_valid = quote();
    apply_effective_prices(
        &mut not_yet_valid,
        &financial_scope(),
        &[effective_price(now - 50, now + 100, now + 200)],
        now,
    )
    .unwrap();
    assert_eq!(not_yet_valid.compute.hourly_rate, None);
    assert_eq!(not_yet_valid.compute.effective_price_override, None);

    let mut valid_from_boundary = quote();
    apply_effective_prices(
        &mut valid_from_boundary,
        &financial_scope(),
        &[effective_price(now - 50, now, now + 100)],
        now,
    )
    .unwrap();
    assert_eq!(
        valid_from_boundary.compute.hourly_rate,
        Some(rust_decimal::Decimal::new(42, 2))
    );

    let mut valid_until_boundary = quote();
    apply_effective_prices(
        &mut valid_until_boundary,
        &financial_scope(),
        &[effective_price(now - 50, now - 100, now)],
        now,
    )
    .unwrap();
    assert_eq!(valid_until_boundary.compute.hourly_rate, None);

    let mut future_observation = quote();
    apply_effective_prices(
        &mut future_observation,
        &financial_scope(),
        &[effective_price(now + 100, now - 100, now + 200)],
        now,
    )
    .unwrap();
    future_observation.refresh_observed_age();
    assert_eq!(
        future_observation.compute.hourly_rate,
        Some(rust_decimal::Decimal::new(42, 2))
    );
    assert_eq!(future_observation.observed_age_seconds, None);

    let mut stale_observation = quote();
    apply_effective_prices(
        &mut stale_observation,
        &financial_scope(),
        &[effective_price(10, now - 100, now + 200)],
        now,
    )
    .unwrap();
    stale_observation.refresh_observed_age();
    assert!(
        stale_observation
            .observed_age_seconds
            .is_some_and(|age| age >= now - 10)
    );
    assert!(
        stale_observation
            .compute
            .effective_price_override
            .as_ref()
            .unwrap()
            .original_observed_at_unix_seconds
            .is_some_and(|observed| observed <= now && observed >= now.saturating_sub(60))
    );
}

#[test]
fn effective_price_requires_exact_scope_and_rejects_included_component() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let price = effective_price(now - 50, now - 100, now + 200);
    let mut changed_scope = financial_scope();
    changed_scope.native_location = "us-west-2".to_owned();
    assert_scope_mismatch(changed_scope, &price, now);
    let mut changed_scope = financial_scope();
    changed_scope.configured_account = "other-account".to_owned();
    assert_scope_mismatch(changed_scope, &price, now);
    let mut changed_scope = financial_scope();
    changed_scope.native_account = "999999999999".to_owned();
    assert_scope_mismatch(changed_scope, &price, now);
    let mut changed_scope = financial_scope();
    changed_scope.image_alias = "other-image".to_owned();
    assert_scope_mismatch(changed_scope, &price, now);
    let mut changed_scope = financial_scope();
    changed_scope.resolved_image = "ami-other".to_owned();
    assert_scope_mismatch(changed_scope, &price, now);
    let mut changed_scope = financial_scope();
    changed_scope.vcpu = 4;
    assert_scope_mismatch(changed_scope, &price, now);
    let mut changed_scope = financial_scope();
    if let ExplanationLaunchFacts::Aws { subnet_id, .. } = &mut changed_scope.launch_facts {
        *subnet_id = "subnet-other".to_owned();
    }
    assert_scope_mismatch(changed_scope, &price, now);
    let mut changed_scope = financial_scope();
    if let ExplanationLaunchFacts::Aws {
        windows_license_included,
        ..
    } = &mut changed_scope.launch_facts
    {
        *windows_license_included = true;
    }
    assert_scope_mismatch(changed_scope, &price, now);

    let mut disk_price = price.clone();
    disk_price.component = EffectivePriceComponent::Disk;
    let mut component_mismatch = quote();
    apply_effective_prices(
        &mut component_mismatch,
        &financial_scope(),
        &[disk_price],
        now,
    )
    .unwrap();
    assert_eq!(component_mismatch.compute.hourly_rate, None);
    assert_eq!(
        component_mismatch.disk.hourly_rate,
        Some(rust_decimal::Decimal::new(42, 2))
    );

    let mut included = quote();
    included.compute.included_in_compute = true;
    assert!(apply_effective_prices(&mut included, &financial_scope(), &[price], now).is_err());
}
