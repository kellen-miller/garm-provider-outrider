use std::collections::BTreeSet;

use outrider::{
    config::{ConfigPatch, GpuRequirement, JobOverrides, RunnerProfile},
    policy::{
        Candidate, NativeCapacityHint, ObservationWindow, Policy, PriceComponent, PriceQuote, rank,
    },
};

use rust_decimal::Decimal;

pub(super) fn central_patch() -> Result<ConfigPatch, outrider::config::ConfigError> {
    load_central_toml(
        "cost_horizon_seconds = 86400\ndefault_policy = \"economy\"\ndefault_purchasing = \"on_demand\"\nmax_price_age_seconds = 3600\nversion = 1\n\n[defaults]\naccounts = [\"shared\"]\narchitecture = \"amd64\"\ncapabilities = [\"docker\"]\nimage = \"ubuntu-24\"\nlocal_scratch_gib = 0\nlocations = [\"us-east-1\"]\nmemory_gib_max = 16\nmemory_gib_min = 8\nos = \"linux\"\nvcpu_max = 8\nvcpu_min = 4\n\n[defaults.boot_disk]\nclass = \"gp3\"\nsize_gib = 40\n\n[defaults.machine_types]\naws = [\"m7i.large\"]\n\n[limits]\nallowed_accounts = [\n    \"shared\",\n    \"build-account\",\n    \"repo-account\",\n]\nallowed_architectures = [\n    \"amd64\",\n    \"arm64\",\n]\nallowed_capabilities = [\"docker\"]\nallowed_clouds = [\n    \"aws\",\n    \"gcp\",\n]\nallowed_gpu_models = []\nallowed_gpu_vendors = []\nallowed_images = [\"ubuntu-24\"]\nallowed_locations = [\"us-east-1\"]\nallowed_os = [\"linux\"]\nallowed_purchasing = [\n    \"on_demand\",\n    \"spot\",\n]\nmax_allocations_account = 100\nmax_allocations_controller = 1000\nmax_allocations_pool = 25\nmax_boot_disk_gib = 1024\nmax_boot_disk_iops = 100000\nmax_boot_disk_throughput_mib_s = 1000\nmax_gpu_count = 8\nmax_gpu_memory_gib = 256\nmax_local_scratch_gib = 512\nmax_memory_gib = 256\nmax_total_cost = 1000\nmax_vcpu = 32\n\n[limits.allowed_machine_families]\n\n[limits.allowed_machine_types]\naws = [\n    \"m7i.large\",\n    \"m7i.xlarge\",\n    \"c7i.xlarge\",\n]\ngcp = [\"n2-standard-4\"]\n\n[policies.economy.missing_data]\naction = \"reject\"\n\n[policies.economy.strategy]\nmax_price_age_seconds = 600\nstrategy = \"economy\"\n\n[profiles.build]\naccounts = [\"build-account\"]\nmemory_gib_max = 16\nmemory_gib_min = 16\nvcpu_max = 8\nvcpu_min = 8\n\n[profiles.build.machine_types]\naws = [\"m7i.xlarge\"]\ngcp = [\"n2-standard-4\"]\n",
    )
}

pub(super) fn profile() -> Result<RunnerProfile, outrider::config::ConfigError> {
    central_patch()
        .and_then(|central| central.normalize_profile(None, "build", None))
        .map(|resolved| resolved.profile)
}

pub(super) fn candidate(identity: &str, price: Decimal) -> Candidate {
    Candidate {
        identity: identity.to_owned(),
        os: "linux".to_owned(),
        architecture: "amd64".to_owned(),
        image: "ubuntu-24".to_owned(),
        vcpu: 8,
        memory_gib: Decimal::from(16),
        machine_type: "m7i.xlarge".to_owned(),
        machine_family: Some("m7i".to_owned()),
        cloud: "aws".to_owned(),
        purchasing: "on_demand".to_owned(),
        account: "build-account".to_owned(),
        location: "us-east-1".to_owned(),
        gpu_vendor: None,
        gpu_model: None,
        gpu_count: 0,
        gpu_memory_gib: None,
        boot_disk_class: "gp3".to_owned(),
        boot_disk_gib: 64,
        boot_disk_iops: 0,
        boot_disk_throughput_mib_s: Decimal::ZERO,
        local_scratch_gib: Decimal::ZERO,
        capabilities: vec!["docker".to_owned()],
        price: Some(PriceQuote {
            provider_request_ids: Vec::new(),
            currency: "USD".to_owned(),
            compute: charge(price),
            disk: charge(Decimal::new(20, 2)),
            licensing: charge(Decimal::ZERO),
            public_address: charge(Decimal::ZERO),
            new_host_minimum_commitment_seconds: None,
            existing_host_remaining_commitment_seconds: None,
            observed_age_seconds: Some(60),
            included_charge_names: BTreeSet::new(),
            exclusions: Vec::new(),
        }),
        interruption_rate_per_hour: Some(Decimal::new(1, 2)),
        allocation_failure_rate: Some(Decimal::new(2, 2)),
        mean_create_seconds: Some(Decimal::from(90)),
        observation_age_seconds: Some(300),
        observation_ages: Some(outrider::policy::ObservationAges {
            create_seconds: Some(300),
            latency_seconds: Some(300),
            interruption_seconds: Some(300),
        }),
        observation_window: Some(ObservationWindow {
            start_unix_seconds: 1_791_996_400,
            end_unix_seconds: 1_792_000_000,
            minimum_samples: 10,
        }),
        observation_samples: 30,
        definitive_create_outcomes: 30,
        measured_latency_samples: 30,
        interruption_samples: 30,
        native_capacity_hint: Some(NativeCapacityHint {
            source: "aws:GetSpotPlacementScores".to_owned(),
            cloud: "aws".to_owned(),
            account: "build-account".to_owned(),
            location: "us-east-1".to_owned(),
            observed_at_unix_seconds: 1_792_000_000,
            score: Decimal::from(7),
        }),
    }
}

#[test]
fn fractional_hardware_caps() {
    let mut ram_candidate = candidate("fractional-ram", Decimal::ONE);
    ram_candidate.memory_gib = Decimal::new(305, 1);
    let mut limits = profile().unwrap();
    limits.memory_gib_min = 0;
    limits.memory_gib_max = 30;
    limits.administrator_limits.max_memory_gib = 30;
    assert_eq!(
        ram_candidate.meets(&limits),
        Err("memory outside allowed range")
    );

    limits.memory_gib_min = 31;
    limits.memory_gib_max = 256;
    limits.administrator_limits.max_memory_gib = 256;
    assert_eq!(
        ram_candidate.meets(&limits),
        Err("memory outside allowed range")
    );

    let mut gpu_candidate = candidate("fractional-gpu", Decimal::ONE);
    gpu_candidate.gpu_vendor = Some("nvidia".to_owned());
    gpu_candidate.gpu_count = 1;
    gpu_candidate.gpu_memory_gib = Some(Decimal::new(165, 1));
    let mut gpu_profile = profile().unwrap();
    gpu_profile.administrator_limits.max_gpu_count = 1;
    gpu_profile.administrator_limits.max_gpu_memory_gib = 16;
    gpu_profile
        .administrator_limits
        .allowed_gpu_vendors
        .insert("nvidia".to_owned());
    gpu_profile.gpu = Some(GpuRequirement {
        vendor: "nvidia".to_owned(),
        model: None,
        count: 1,
        memory_gib: None,
    });
    assert_eq!(
        gpu_candidate.meets(&gpu_profile),
        Err("GPU not permitted by administrator")
    );

    let mut scratch_candidate = candidate("fractional-scratch", Decimal::ONE);
    scratch_candidate.local_scratch_gib = Decimal::new(85, 1);
    let mut scratch_profile = profile().unwrap();
    scratch_profile.administrator_limits.max_local_scratch_gib = 8;
    assert_eq!(
        scratch_candidate.meets(&scratch_profile),
        Err("local scratch outside allowed range")
    );

    let mut unknown_vram = gpu_candidate;
    unknown_vram.gpu_memory_gib = None;
    assert_eq!(
        unknown_vram.meets(&gpu_profile),
        Err("GPU not permitted by administrator")
    );
    unknown_vram.gpu_memory_gib = Some(Decimal::ZERO);
    assert_eq!(
        unknown_vram.meets(&gpu_profile),
        Err("GPU not permitted by administrator")
    );
    unknown_vram.gpu_memory_gib = Some(-Decimal::ONE);
    assert_eq!(
        unknown_vram.meets(&gpu_profile),
        Err("GPU not permitted by administrator")
    );

    let exact_local_scratch = Decimal::from(900u32)
        .checked_mul(Decimal::from(1_000_000_000u64))
        .and_then(|bytes| bytes.checked_div(Decimal::from(1_073_741_824u64)))
        .unwrap();
    assert_eq!(
        exact_local_scratch,
        Decimal::from_str_exact("838.1903171539306640625").unwrap()
    );
}

#[test]
fn native_capacity_hints_are_advisory() {
    let low_price = candidate("low-price", Decimal::new(5, 2));
    let mut high_score = candidate("high-score", Decimal::ONE);
    let mut low_score = low_price;
    high_score.native_capacity_hint.as_mut().unwrap().score = Decimal::from(10);
    low_score.native_capacity_hint.as_mut().unwrap().score = Decimal::ONE;
    let policy = Policy::Economy {
        max_price_age_seconds: 600,
    };

    let ranked = rank(&policy, &[high_score, low_score], &profile().unwrap()).unwrap();
    assert_eq!(ranked[0].identity, "low-price");
    assert_eq!(
        ranked[0].native_capacity_hint.as_ref().unwrap().score,
        Decimal::ONE
    );
}

pub(super) fn charge(rate: Decimal) -> PriceComponent {
    PriceComponent {
        hourly_rate: Some(rate),
        minimum_commitment_seconds: 0,
        source: "synthetic".to_owned(),
        observed_at_unix_seconds: Some(1_792_000_000),
        effective_at_unix_seconds: Some(1_792_000_000),
        included_in_compute: false,
        exclusions: Vec::new(),
        effective_price_override: None,
    }
}

#[test]
fn quote_age_rejects_future_required_observations_and_ignores_included_markers() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut quote = PriceQuote {
        provider_request_ids: Vec::new(),
        currency: "USD".to_owned(),
        compute: charge(Decimal::ONE),
        disk: charge(Decimal::ONE),
        licensing: PriceComponent {
            hourly_rate: Some(Decimal::ZERO),
            included_in_compute: true,
            observed_at_unix_seconds: None,
            ..charge(Decimal::ZERO)
        },
        public_address: charge(Decimal::ZERO),
        new_host_minimum_commitment_seconds: None,
        existing_host_remaining_commitment_seconds: None,
        observed_age_seconds: Some(0),
        included_charge_names: BTreeSet::new(),
        exclusions: Vec::new(),
    };
    quote.disk.observed_at_unix_seconds = Some(now.saturating_add(60));
    quote.refresh_observed_age();
    assert_eq!(quote.observed_age_seconds, None);

    quote.disk.observed_at_unix_seconds = Some(now.saturating_sub(5));
    quote.refresh_observed_age();
    assert!(quote.observed_age_seconds.is_some());
}

#[test]
fn precedence_replace_lists() {
    outrider::config::CentralConfig::from_toml(include_str!("../../examples/central-runtime.toml"))
        .unwrap();

    let example_central = load_central_toml(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/central.toml"
        ))
        .unwrap(),
    )
    .unwrap();
    let example_repository = ConfigPatch::from_toml(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/repository.toml"
        ))
        .unwrap(),
    )
    .unwrap();
    assert!(
        example_central
            .normalize_profile(Some(&example_repository), "linux-large", None)
            .is_ok()
    );
    assert_eq!(
        example_central
            .normalize_profile(Some(&example_repository), "linux-large", None)
            .unwrap()
            .profile
            .vcpu_min,
        8
    );

    let central = central_patch().unwrap();
    let local = ConfigPatch::from_toml(
        "version = 1\n\n[profiles.build]\naccounts = [\"repo-account\"]\n\n[profiles.build.machine_types]\naws = [\"c7i.xlarge\"]\n",
    )
    .unwrap();
    let resolved = central
        .normalize_profile(Some(&local), "build", None)
        .unwrap();
    assert_eq!(resolved.profile.accounts, ["repo-account"]);
    assert_eq!(resolved.profile.machine_types["aws"], ["c7i.xlarge"]);
    assert_eq!(resolved.profile.machine_types["gcp"], ["n2-standard-4"]);
    assert_eq!(resolved.profile.memory_gib_min, 16);
    assert!(ConfigPatch::from_toml("version = 2\n").is_err());
    assert!(ConfigPatch::from_toml("[profiles]\n").is_err());
    let mut missing_version = central.clone();
    missing_version.version = None;
    assert!(
        missing_version
            .normalize_profile(None, "build", None)
            .is_err()
    );
    let mut centrally_capped = central.clone();
    centrally_capped
        .defaults
        .as_mut()
        .unwrap()
        .maximum_total_cost = Some(Decimal::from(5));
    assert_eq!(
        centrally_capped
            .normalize_profile(Some(&local), "build", None)
            .unwrap()
            .profile
            .maximum_total_cost,
        Some(Decimal::from(5))
    );
    assert!(ConfigPatch::from_toml("version = 1\nversion = 1\n").is_err());
    assert!(ConfigPatch::from_toml("version = 1\ndefaults = null\n").is_err());
    assert!(ConfigPatch::from_toml("version: 1\n").is_err());
    assert!(ConfigPatch::from_toml("not_a_field = true\nversion = 1\n").is_err());
    for malformed_policy in [
        "version = 1\n[policies.economy]\nstrategy = {strategy = 'economy', max_price_age_seconds = 600, typo = true}\nmissing_data = {action = 'reject'}\n",
        "version = 1\n[policies.economy]\nstrategy = {strategy = 'economy', max_price_age_seconds = 600}\nmissing_data = {action = 'reject', typo = true}\n",
        "version = 1\n\n[policies.economy.missing_data]\naction = \"reject\"\n\n[policies.economy.strategy]\nmax_price_age_seconds = 600\nstrategy = \"economy\"\ntypo = true\n",
        "version = 1\n\n[policies.economy.missing_data]\naction = \"reject\"\ntypo = true\n\n[policies.economy.strategy]\nmax_price_age_seconds = 600\nstrategy = \"economy\"\n",
        "version = 1\n\n[policies.economy]\ntypo = true\n\n[policies.economy.missing_data]\naction = \"reject\"\n\n[policies.economy.strategy]\nmax_price_age_seconds = 600\nstrategy = \"economy\"\n",
        "version = 1\n\n[policies.economy.missing_data]\naction = \"reject\"\n\n[policies.economy.strategy]\nduration_seconds = 3600\nmax_price_age_seconds = 600\nstrategy = \"economy\"\n",
        "version = 1\n\n[policies.weighted.missing_data]\naction = \"reject\"\n\n[policies.weighted.strategy]\ncost_scale = 1\ncost_weight = 1\nduration_seconds = 3600\nfailure_weight = 0\ninterruption_scale = 1\ninterruption_weight = 0\nlatency_scale = 1\nlatency_weight = 0\nmax_observation_age_seconds = 600\nmax_price_age_seconds = 600\nmin_samples = 10\nstrategy = \"weighted\"\n",
    ] {
        assert!(matches!(
            outrider::config::CentralConfig::from_toml(malformed_policy),
            Err(outrider::config::ConfigError::UnknownField(_))
        ));
    }
}

#[test]
fn hard_limit_reject() {
    let local = ConfigPatch::from_toml("version = 1\n\n[profiles.build]\nvcpu_max = 64\n").unwrap();
    assert!(matches!(
        central_patch()
            .unwrap()
            .normalize_profile(Some(&local), "build", None),
        Err(outrider::config::ConfigError::HardLimit(_))
    ));
    let forbidden = JobOverrides {
        profile: Default::default(),
        network_id: Some("subnet-secret".to_owned()),
        ..JobOverrides::default()
    };

    assert!(matches!(
        central_patch()
            .unwrap()
            .normalize_profile(None, "build", Some(&forbidden)),
        Err(outrider::config::ConfigError::ForbiddenOverride(_))
    ));
    assert!(matches!(
        central_patch()
            .unwrap()
            .normalize_profile(None, "unknown", None),
        Err(outrider::config::ConfigError::MissingProfile(_))
    ));
    let cost_bypass =
        ConfigPatch::from_toml("version = 1\n\n[profiles.build]\nmaximum_total_cost = 1001\n")
            .unwrap();
    assert!(matches!(
        central_patch()
            .unwrap()
            .normalize_profile(Some(&cost_bypass), "build", None),
        Err(outrider::config::ConfigError::HardLimit(_))
    ));
    let clear_selectors =
        ConfigPatch::from_toml("version = 1\n\n[profiles.build]\naccounts = []\nlocations = []\n")
            .unwrap();
    let mut outside_admin_scope = candidate("outside", Decimal::ONE);
    outside_admin_scope.account = "unlisted-account".to_owned();
    let normalized = central_patch()
        .unwrap()
        .normalize_profile(Some(&clear_selectors), "build", None)
        .unwrap();
    assert_eq!(
        outside_admin_scope.meets(&normalized.profile).unwrap_err(),
        "account not permitted"
    );
    let mut model_restricted = profile().unwrap();
    model_restricted
        .administrator_limits
        .allowed_gpu_vendors
        .insert("nvidia".to_owned());
    model_restricted
        .administrator_limits
        .allowed_gpu_models
        .insert("a100".to_owned());
    let mut unknown_gpu_model = candidate("unknown-gpu", Decimal::ONE);
    unknown_gpu_model.gpu_count = 1;
    unknown_gpu_model.gpu_vendor = Some("nvidia".to_owned());
    assert_eq!(
        unknown_gpu_model.meets(&model_restricted).unwrap_err(),
        "GPU not permitted by administrator"
    );
}

#[test]
fn repository_admin_settings_rejected() {
    let central = central_patch().unwrap();
    let repository_base = ConfigPatch::from_toml("version = 1\n\n[profiles.build]\n").unwrap();
    assert!(
        central
            .normalize_profile(Some(&repository_base), "build", None)
            .is_ok()
    );

    let mut repository = repository_base.clone();
    repository.defaults = central.defaults.clone();
    assert!(matches!(
        central.normalize_profile(Some(&repository), "build", None),
        Err(outrider::config::ConfigError::ForbiddenOverride(_))
    ));
    assert!(ConfigPatch::from_toml("version = 1\neffective_prices = []\n").is_err());
    let mut repository = repository_base.clone();
    repository.limits = central.limits.clone();
    assert!(matches!(
        central.normalize_profile(Some(&repository), "build", None),
        Err(outrider::config::ConfigError::ForbiddenOverride(_))
    ));
    let mut repository = repository_base.clone();
    repository.default_purchasing = Some("spot".to_owned());
    assert!(matches!(
        central.normalize_profile(Some(&repository), "build", None),
        Err(outrider::config::ConfigError::ForbiddenOverride(_))
    ));
    let mut repository = repository_base.clone();
    repository.control = Some(outrider::config::ControlSettings {
        listen_address: "127.0.0.1:1".to_owned(),
        database_path: "db".into(),
        auth_token_file: "token".into(),
        provider_timeout_seconds: 1,
    });
    assert!(matches!(
        central.normalize_profile(Some(&repository), "build", None),
        Err(outrider::config::ConfigError::ForbiddenOverride(_))
    ));
    let mut repository = repository_base.clone();
    repository.provider_bindings = Some(Vec::new());
    assert!(matches!(
        central.normalize_profile(Some(&repository), "build", None),
        Err(outrider::config::ConfigError::ForbiddenOverride(_))
    ));
    let mut repository = repository_base.clone();
    repository.aws_catalog_bindings = Some(Vec::new());
    assert!(matches!(
        central.normalize_profile(Some(&repository), "build", None),
        Err(outrider::config::ConfigError::ForbiddenOverride(_))
    ));
    let schema = outrider::config::json_schema();
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["providers"]["items"]["$ref"],
        "#/$defs/ProviderSource"
    );
    let forbidden_job = JobOverrides {
        profile: Default::default(),
        network_id: Some("subnet-job".to_owned()),
        credentials: Some("credential-job".to_owned()),
        ..JobOverrides::default()
    };

    assert!(matches!(
        central.normalize_profile(None, "build", Some(&forbidden_job)),
        Err(outrider::config::ConfigError::ForbiddenOverride(_))
    ));
}

pub(super) fn load_central_toml(toml: &str) -> Result<ConfigPatch, outrider::config::ConfigError> {
    outrider::config::CentralConfig::from_toml(toml)?
        .load_native_configuration(&std::collections::BTreeMap::new())
}
