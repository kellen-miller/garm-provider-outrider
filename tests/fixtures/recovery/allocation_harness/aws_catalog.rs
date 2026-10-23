use super::*;

pub(crate) struct AllocationHarness {
    pub(crate) _fixture: JournalFixture,
    pub(crate) journal: HarnessJournal,
    pub(crate) service: HarnessService,
    pub(crate) old_config: ConfigPatch,
    pub(crate) current_config: ConfigPatch,
    pub(crate) aws_endpoint: String,
    pub(crate) request: ProviderRequest,
    pub(crate) lifecycle_log: PathBuf,
    pub(crate) record_path: PathBuf,
    pub(crate) compute_quote_requests: Arc<AtomicU64>,
    pub(crate) native_visible: Arc<AtomicBool>,
    pub(crate) collision_native_observations: Arc<AtomicU64>,
    pub(crate) native_inspection_mode: Arc<AtomicU64>,
    pub(crate) native_inspections: Arc<AtomicU64>,
    pub(crate) _native_inspection_delay_ms: Arc<AtomicU64>,
    pub(crate) _delayed_native_inspections: Arc<AtomicU64>,
    pub(crate) _image_requests: Arc<AtomicU64>,
    pub(crate) _delayed_image_requests: Arc<AtomicU64>,
    pub(crate) _image_delay_on_request: Arc<AtomicU64>,
    pub(crate) _image_request_delay_ms: Arc<AtomicU64>,
    pub(crate) _request_trace: Arc<Mutex<Vec<String>>>,
    pub(crate) quote_outage: Arc<AtomicBool>,
    pub(crate) server_task: tokio::task::JoinHandle<std::io::Result<()>>,
    pub spot_event_time: Arc<Mutex<String>>,
    pub spot_conflict: Arc<AtomicBool>,
    pub spot_slow_first: Arc<AtomicBool>,
}

pub(crate) struct HarnessJournal(pub(crate) Option<Arc<Journal>>);

pub(crate) struct HarnessService(pub(crate) Option<AllocationService>);

pub(crate) struct AllocationHarnessOptions {
    pub(crate) current_max_vcpu: u32,
    pub(crate) current_account_allowed: bool,
    pub(crate) max_allocations_account: u32,
    pub(crate) snapshot_state: String,
    pub(crate) expires_in_seconds: i64,
    pub(crate) allocation_timeout_seconds: u64,
    pub(crate) provider_timeout_seconds: u64,
    pub(crate) child_mode: String,
    pub(crate) spot: bool,
}

pub(crate) struct HarnessQuotePolicy {
    pub(crate) ordered: bool,
    pub(crate) cost_ceiling: bool,
    pub(crate) subnet_metadata_changes: bool,
    pub(crate) max_price_age_seconds: Option<u64>,
    pub(crate) economy_max_price_age_seconds: Option<u64>,
}

impl HarnessQuotePolicy {
    pub(crate) const DEFAULT: Self = Self {
        ordered: false,
        cost_ceiling: false,
        subnet_metadata_changes: false,
        max_price_age_seconds: None,
        economy_max_price_age_seconds: None,
    };
}

pub(crate) async fn aws_allocation_backend(
    endpoint: &str,
    config: &ConfigPatch,
    catalog_id: &str,
) -> AwsBackend {
    let catalog = config
        .aws_catalog_bindings
        .as_ref()
        .unwrap_or_else(|| panic!("AWS catalog"))
        .iter()
        .find(|catalog| catalog.id == catalog_id)
        .unwrap_or_else(|| panic!("configured AWS catalog"));
    let binding = config
        .provider_bindings
        .as_ref()
        .unwrap_or_else(|| panic!("provider bindings"))
        .iter()
        .find(|binding| binding.id == catalog.provider_binding_id)
        .unwrap_or_else(|| panic!("catalog provider binding"));
    let mut settings = AwsBackend::settings_from_bindings(catalog, binding, false)
        .unwrap_or_else(|_| panic!("AWS catalog and child binding match"));
    settings.ec2_endpoint_url = Some(endpoint.to_owned());
    settings.pricing_endpoint_url = Some(endpoint.to_owned());
    settings.sts_endpoint_url = Some(endpoint.to_owned());
    let shared = aws_config::defaults(BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            "AKIDTESTAWSCONTRACT",
            "secret-test-only-not-a-real-key",
            None,
            None,
            "allocation-contract-test",
        ))
        .load()
        .await;
    let ec2 = aws_sdk_ec2::Client::from_conf(
        aws_sdk_ec2::config::Builder::from(&shared)
            .endpoint_url(endpoint)
            .build(),
    );
    let pricing = aws_sdk_pricing::Client::from_conf(
        aws_sdk_pricing::config::Builder::from(&shared)
            .region(aws_config::Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .build(),
    );
    let sts = aws_sdk_sts::Client::from_conf(
        aws_sdk_sts::config::Builder::from(&shared)
            .endpoint_url(endpoint)
            .build(),
    );
    AwsBackend::with_clients(
        settings,
        ec2,
        pricing,
        sts,
        tokio::time::Instant::now() + std::time::Duration::from_secs(10),
    )
    .await
    .unwrap_or_else(|_| panic!("construct concrete AWS catalog"))
}

pub(crate) fn configure_ordered_quote_policy(
    config: &mut ConfigPatch,
    quote_policy: &HarnessQuotePolicy,
) {
    let mut identities = Vec::new();
    for catalog_id in ["aws-east", "aws-east-duplicate"] {
        for image in ["oldAMI", "newAMI"] {
            identities.push(format!(
                "aws|{catalog_id}|synthetic-account|us-east-1|us-east-1|m7i.xlarge|{image}|on_demand"
            ));
        }
    }
    config.default_policy = Some("ordered-contract".to_owned());
    config.policies = Some(BTreeMap::from([(
        "ordered-contract".to_owned(),
        PolicyDefinition {
            strategy: Policy::Ordered { identities },
            missing_data: MissingDataBehavior::Reject,
        },
    )]));
    if let Some(limits) = &mut config.limits {
        limits.max_total_cost = quote_policy
            .cost_ceiling
            .then_some(rust_decimal::Decimal::new(1000, 0));
    }
    if let Some(defaults) = &mut config.defaults {
        defaults.maximum_total_cost = None;
    }
}

pub(crate) fn configure_harness_timeouts(
    config: &mut ConfigPatch,
    provider_timeout_seconds: u64,
    allocation_timeout_seconds: u64,
) {
    config
        .control
        .as_mut()
        .unwrap_or_else(|| panic!("missing control"))
        .provider_timeout_seconds = provider_timeout_seconds;
    config
        .ingress
        .as_mut()
        .unwrap_or_else(|| panic!("missing ingress"))
        .allocation_timeout_seconds = allocation_timeout_seconds;
}

pub(crate) async fn rebuild_allocation_service(
    fixture: &JournalFixture,
    journal: Arc<Journal>,
    endpoint: &str,
    old_config: &ConfigPatch,
    current_config: &ConfigPatch,
    child_mode: &str,
) -> AllocationService {
    let old_bindings = old_config
        .provider_bindings
        .as_ref()
        .unwrap_or_else(|| panic!("retained provider bindings"));
    let binding = old_bindings
        .iter()
        .find(|binding| binding.id == "aws-child")
        .unwrap_or_else(|| panic!("retained AWS child"));
    let alias_binding = old_bindings
        .iter()
        .find(|binding| binding.id == "aws-child-alias")
        .unwrap_or_else(|| panic!("retained alias child"));
    assert_eq!(
        binding.executable_path,
        allocation_binding(fixture, child_mode).executable_path
    );
    let old_backend = Box::pin(aws_allocation_backend(endpoint, old_config, "aws-east")).await;
    let current_backend =
        Box::pin(aws_allocation_backend(endpoint, current_config, "aws-east")).await;
    let old_duplicate_backend = Box::pin(aws_allocation_backend(
        endpoint,
        old_config,
        "aws-east-duplicate",
    ))
    .await;
    let current_duplicate_backend = Box::pin(aws_allocation_backend(
        endpoint,
        current_config,
        "aws-east-duplicate",
    ))
    .await;
    let old_fingerprint = catalog_configuration_fingerprint(old_config, "aws", "aws-east")
        .unwrap_or_else(|_| panic!("old catalog fingerprint"));
    let current_fingerprint = catalog_configuration_fingerprint(current_config, "aws", "aws-east")
        .unwrap_or_else(|_| panic!("current catalog fingerprint"));
    let old_duplicate_fingerprint =
        catalog_configuration_fingerprint(old_config, "aws", "aws-east-duplicate")
            .unwrap_or_else(|_| panic!("old duplicate catalog fingerprint"));
    let current_duplicate_fingerprint =
        catalog_configuration_fingerprint(current_config, "aws", "aws-east-duplicate")
            .unwrap_or_else(|_| panic!("current duplicate catalog fingerprint"));
    let process_environment = BTreeMap::from([
        (
            OsString::from("OUTRIDER_NATIVE_LOCATOR"),
            OsString::from("i-synthetic-001"),
        ),
        (
            OsString::from("OUTRIDER_CREATE_BARRIER"),
            fixture
                .directory
                .join("create-barrier")
                .as_os_str()
                .to_owned(),
        ),
        (
            OsString::from("OUTRIDER_FIXTURE_MODE"),
            OsString::from(child_mode),
        ),
        (
            OsString::from("OUTRIDER_RECORD"),
            fixture
                .directory
                .join("provider-record.json")
                .into_os_string(),
        ),
        (
            OsString::from("OUTRIDER_LIFECYCLE_LOG"),
            fixture
                .directory
                .join("provider-lifecycle.jsonl")
                .into_os_string(),
        ),
    ]);
    let registry = ProviderRegistry::new(
        vec![binding.clone(), alias_binding.clone()],
        process_environment.clone(),
        ProcessLimits::default(),
    )
    .unwrap_or_else(|_| panic!("rebuild pinned provider registry"));
    AllocationService::new(
        current_config.clone(),
        journal,
        vec![
            ConnectedCatalog::aws(
                old_backend,
                old_fingerprint,
                "aws-east".to_owned(),
                binding.id.clone(),
                binding.account.clone(),
                binding.location.clone(),
            ),
            ConnectedCatalog::aws(
                current_backend,
                current_fingerprint,
                "aws-east".to_owned(),
                binding.id.clone(),
                binding.account.clone(),
                binding.location.clone(),
            ),
            ConnectedCatalog::aws(
                old_duplicate_backend,
                old_duplicate_fingerprint,
                "aws-east-duplicate".to_owned(),
                alias_binding.id.clone(),
                binding.account.clone(),
                binding.location.clone(),
            ),
            ConnectedCatalog::aws(
                current_duplicate_backend,
                current_duplicate_fingerprint,
                "aws-east-duplicate".to_owned(),
                alias_binding.id.clone(),
                binding.account.clone(),
                binding.location.clone(),
            ),
        ],
        registry,
        process_environment,
    )
    .unwrap_or_else(|_| panic!("rebuild allocation service"))
}

pub(crate) fn snapshot_document(central: ConfigPatch, request_token: &str) -> SnapshotDocument {
    let resolved_profile = central
        .normalize_profile(None, "linux-large", None)
        .unwrap_or_else(|_| panic!("normalize snapshot profile"))
        .profile;
    SnapshotDocument {
        base_repository: RepositoryRef {
            id: 7,
            name: "outrider".to_owned(),
            full_name: "example/outrider".to_owned(),
            owner: OwnerRef {
                login: "example".to_owned(),
            },
        },
        base_repository_id: 7,
        source_repository_id: 7,
        run_id: 11,
        run_attempt: 1,
        job_id: 13,
        commit_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        central,
        repository: ConfigPatch::default(),
        resolved_profile,
        image_catalogs: vec![json!({"request_token": request_token})],
    }
}
