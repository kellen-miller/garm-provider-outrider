use super::*;

impl AllocationHarness {
    pub(crate) async fn new(options: AllocationHarnessOptions) -> Self {
        Self::new_with_quote_policy(options, HarnessQuotePolicy::DEFAULT).await
    }

    pub(crate) async fn new_with_quote_policy(
        options: AllocationHarnessOptions,
        quote_policy: HarnessQuotePolicy,
    ) -> Self {
        let fixture = JournalFixture::new();
        let mut binding = allocation_binding(&fixture, &options.child_mode);
        if options.spot {
            binding
                .purchasing_capabilities
                .insert(PurchasingCapability::Spot);
        }
        let mut old_config = allocation_central(&binding, "oldAMI");
        let mut current_config = allocation_central(&binding, "newAMI");
        configure_harness_timeouts(
            &mut old_config,
            options.provider_timeout_seconds,
            options.allocation_timeout_seconds,
        );
        configure_harness_timeouts(
            &mut current_config,
            options.provider_timeout_seconds,
            options.allocation_timeout_seconds,
        );
        if quote_policy.ordered {
            configure_ordered_quote_policy(&mut old_config, &quote_policy);
            configure_ordered_quote_policy(&mut current_config, &quote_policy);
        }
        configure_price_age_policy(&mut old_config, &quote_policy);
        configure_price_age_policy(&mut current_config, &quote_policy);
        let limits = current_config
            .limits
            .as_mut()
            .unwrap_or_else(|| panic!("current limits"));
        limits.max_vcpu = options.current_max_vcpu;
        limits.max_allocations_account = options.max_allocations_account;
        if !options.current_account_allowed {
            limits.allowed_accounts.remove("synthetic-account");
        }
        let snapshot_config = old_config.clone();
        let mut snapshot = snapshot_document(snapshot_config, "route-token");
        if options.spot {
            snapshot.resolved_profile.purchasing_models = BTreeSet::from(["spot".to_owned()]);
        }
        let journal = Arc::new(fixture.journal());
        journal
            .save_pool_snapshot(&PoolSnapshot {
                id: "snapshot-id".to_owned(),
                repository_id: 7,
                request_token: "route-token".to_owned(),
                config_json: serde_json::to_string(&snapshot)
                    .unwrap_or_else(|_| panic!("serialize snapshot")),
                state: options.snapshot_state,
                garm_pool_id: Some("pool-1".to_owned()),
                expires_at: (unix_now_for_test() as i64 + options.expires_in_seconds).max(0) as u64,
            })
            .unwrap_or_else(|_| panic!("persist snapshot"));

        let server = Box::pin(aws_server(
            options.child_mode != "no_stdin_reader",
            options.spot,
            None,
            SubnetMetadataBehavior::from(quote_policy.subnet_metadata_changes),
        ))
        .await;
        let old_backend = Box::pin(aws_allocation_backend(
            &server.endpoint,
            &old_config,
            "aws-east",
        ))
        .await;
        let current_backend = Box::pin(aws_allocation_backend(
            &server.endpoint,
            &current_config,
            "aws-east",
        ))
        .await;
        let old_duplicate_backend = Box::pin(aws_allocation_backend(
            &server.endpoint,
            &old_config,
            "aws-east-duplicate",
        ))
        .await;
        let current_duplicate_backend = Box::pin(aws_allocation_backend(
            &server.endpoint,
            &current_config,
            "aws-east-duplicate",
        ))
        .await;
        let old_fingerprint = catalog_configuration_fingerprint(&old_config, "aws", "aws-east")
            .unwrap_or_else(|_| panic!("old catalog fingerprint"));
        let current_fingerprint =
            catalog_configuration_fingerprint(&current_config, "aws", "aws-east")
                .unwrap_or_else(|_| panic!("current catalog fingerprint"));
        let old_duplicate_fingerprint =
            catalog_configuration_fingerprint(&old_config, "aws", "aws-east-duplicate")
                .unwrap_or_else(|_| panic!("old duplicate catalog fingerprint"));
        let current_duplicate_fingerprint =
            catalog_configuration_fingerprint(&current_config, "aws", "aws-east-duplicate")
                .unwrap_or_else(|_| panic!("current duplicate catalog fingerprint"));
        let record_path = fixture.directory.join("provider-record.json");
        let lifecycle_log = fixture.directory.join("provider-lifecycle.jsonl");
        let alias_binding = old_config
            .provider_bindings
            .as_ref()
            .unwrap_or_else(|| panic!("provider bindings"))
            .iter()
            .find(|binding| binding.id == "aws-child-alias")
            .unwrap_or_else(|| panic!("alias provider binding"));
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
                OsString::from(options.child_mode),
            ),
            (
                OsString::from("OUTRIDER_RECORD"),
                record_path.as_os_str().to_owned(),
            ),
            (
                OsString::from("OUTRIDER_LIFECYCLE_LOG"),
                lifecycle_log.as_os_str().to_owned(),
            ),
        ]);
        let registry = ProviderRegistry::new(
            vec![binding.clone(), alias_binding.clone()],
            process_environment.clone(),
            ProcessLimits::default(),
        )
        .unwrap_or_else(|_| panic!("construct pinned provider registry"));
        let service = AllocationService::new(
            current_config.clone(),
            journal.clone(),
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
        .unwrap_or_else(|_| panic!("construct allocation service with retained catalogs"));
        let request = ProviderRequest {
            command: "CreateInstance".to_owned(),
            interface_version: outrider::providers::GARM_INTERFACE_VERSION.to_owned(),
            controller_id: "controller-1".to_owned(),
            pool_id: Some("pool-1".to_owned()),
            instance_id: None,
            bootstrap: Some(BootstrapInstance {
                name: "runner-old-image".to_owned(),
                tools: vec![RunnerApplicationDownload {
                    os: Some("linux".to_owned()),
                    architecture: Some("x64".to_owned()),
                    download_url: Some(
                        "https://github.com/actions/runner/releases/download/v1/runner.tar.gz"
                            .to_owned(),
                    ),
                    filename: Some("runner.tar.gz".to_owned()),
                    temp_download_token: Some("synthetic-download-token".to_owned()),
                    sha256_checksum: Some("a".repeat(64)),
                }],
                repo_url: "https://github.com/example/outrider".to_owned(),
                callback_url: "https://garm.example/callback".to_owned(),
                metadata_url: "http://127.0.0.1/metadata".to_owned(),
                instance_token: "synthetic-callback-token".to_owned(),
                ssh_keys: Vec::new(),
                extra_specs: None,
                github_runner_group: String::new(),
                ca_cert_bundle: None,
                os_arch: "amd64".to_owned(),
                os_type: "linux".to_owned(),
                flavor: String::new(),
                image: String::new(),
                pool_id: "pool-1".to_owned(),
                labels: vec![
                    "route-token".to_owned(),
                    "runner-pool-id=pool-1".to_owned(),
                    "runner-controller-id=controller-1".to_owned(),
                ],
                user_data_options: UserDataOptions::default(),
                jit_config_enabled: false,
            }),
            pool_extra_specs: Some(json!({"snapshot_id":"snapshot-id"})),
        };
        Self {
            _fixture: fixture,
            journal: HarnessJournal(Some(journal)),
            service: HarnessService(Some(service)),
            old_config,
            current_config,
            aws_endpoint: server.endpoint,
            request,
            lifecycle_log,
            record_path,
            compute_quote_requests: server.compute_quote_requests,
            native_visible: server.native_visible,
            collision_native_observations: server.collision_native_observations,
            native_inspection_mode: server.native_inspection_mode,
            native_inspections: server.native_inspections,
            _native_inspection_delay_ms: server.native_inspection_delay_ms,
            _delayed_native_inspections: server.delayed_native_inspections,
            _image_requests: server.image_requests,
            _delayed_image_requests: server.delayed_image_requests,
            _image_delay_on_request: server.image_delay_on_request,
            _image_request_delay_ms: server.image_request_delay_ms,
            _request_trace: server.request_trace,
            quote_outage: server.quote_outage,
            server_task: server.task,
            spot_event_time: server.spot_event_time,
            spot_conflict: server.spot_conflict,
            spot_slow_first: server.spot_slow_first,
        }
    }

    pub(crate) fn allocation(&self) -> outrider::journal::AllocationRecord {
        self.journal
            .allocation("controller-1", "runner-old-image")
            .unwrap_or_else(|_| panic!("lookup allocation"))
            .unwrap_or_else(|| panic!("allocation row exists"))
    }

    pub(crate) fn lifecycle_commands(&self) -> Vec<String> {
        fs::read_to_string(&self.lifecycle_log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|record| record["command"].as_str().map(str::to_owned))
            .collect()
    }
}

pub(crate) fn configure_price_age_policy(
    config: &mut ConfigPatch,
    quote_policy: &HarnessQuotePolicy,
) {
    if let Some(maximum_age) = quote_policy.max_price_age_seconds {
        config.max_price_age_seconds = Some(maximum_age);
    }
    if let Some(maximum_age) = quote_policy.economy_max_price_age_seconds {
        let policies = config
            .policies
            .as_mut()
            .unwrap_or_else(|| panic!("central policies"));
        let economy = policies
            .get_mut("economy")
            .unwrap_or_else(|| panic!("economy policy"));
        economy.strategy = Policy::Economy {
            max_price_age_seconds: maximum_age,
        };
    }
}

impl Drop for AllocationHarness {
    fn drop(&mut self) {
        self.server_task.abort();
    }
}

impl Deref for HarnessJournal {
    type Target = Arc<Journal>;

    fn deref(&self) -> &Self::Target {
        self.0
            .as_ref()
            .unwrap_or_else(|| panic!("journal is available in this harness phase"))
    }
}

impl Deref for HarnessService {
    type Target = AllocationService;

    fn deref(&self) -> &Self::Target {
        self.0
            .as_ref()
            .unwrap_or_else(|| panic!("allocation service is available in this harness phase"))
    }
}

pub(crate) fn unix_now_for_test() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) async fn release_fast_delete_after_native_absence(
    harness: &AllocationHarness,
    ready_path: &Path,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !ready_path.exists() {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    harness.native_visible.store(false, Ordering::Relaxed);
    let release_path = PathBuf::from(format!("{}.release", ready_path.display()));
    fs::write(release_path, b"release").unwrap_or_else(|_| panic!("release fast cleanup"));
}

pub(crate) fn assert_restart_cleanup_effects(harness: &AllocationHarness) {
    let records = fs::read_to_string(&harness.lifecycle_log)
        .unwrap_or_else(|_| panic!("read recovery child records"));
    let deletes = records
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record["command"] == "DeleteInstance")
        .collect::<Vec<_>>();
    assert_eq!(deletes.len(), 2);
    let ids = deletes
        .iter()
        .filter_map(|record| record["instance_id"].as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids, BTreeSet::from(["i-synthetic-001", "i-synthetic-002"]));
    let launch_gap = (deletes[0]["at_monotonic"]
        .as_f64()
        .unwrap_or_else(|| panic!("first cleanup launch time"))
        - deletes[1]["at_monotonic"]
            .as_f64()
            .unwrap_or_else(|| panic!("second cleanup launch time")))
    .abs();
    assert!(
        launch_gap < 0.5,
        "independent child cleanup is polled fairly"
    );
    let allocations = harness
        .journal
        .allocations_for_controller("controller-1")
        .unwrap_or_else(|_| panic!("read recovered allocations"));
    let fast = allocations
        .iter()
        .find(|allocation| allocation.runner_name == "runner-second")
        .unwrap_or_else(|| panic!("fast allocation"));
    let slow = allocations
        .iter()
        .find(|allocation| allocation.runner_name == "runner-old-image")
        .unwrap_or_else(|| panic!("slow allocation"));
    assert_eq!(fast.state, "deleted");
    assert_eq!(slow.state, "deleting");
    assert_eq!(harness.allocation().state, "deleting");
    assert!(
        harness
            .lifecycle_commands()
            .contains(&"DeleteInstance".to_owned())
    );
    assert!(harness.compute_quote_requests.load(Ordering::Relaxed) > 0);
    assert_eq!(
        harness
            .collision_native_observations
            .load(Ordering::Relaxed),
        0
    );
    assert_eq!(harness.native_inspection_mode.load(Ordering::Relaxed), 0);
    assert!(harness.native_inspections.load(Ordering::Relaxed) > 0);
    assert!(!harness.quote_outage.load(Ordering::Relaxed));
    assert!(!harness.record_path.exists());
}

impl Default for AllocationHarnessOptions {
    fn default() -> Self {
        Self {
            current_max_vcpu: 32,
            current_account_allowed: true,
            max_allocations_account: 100,
            snapshot_state: "enabled".to_owned(),
            expires_in_seconds: 3600,
            allocation_timeout_seconds: 600,
            provider_timeout_seconds: 5,
            child_mode: "normal".to_owned(),
            spot: false,
        }
    }
}

pub(crate) async fn restart_retained_cleanup_case() {
    let mut harness = AllocationHarness::new(AllocationHarnessOptions {
        child_mode: "slow_first_delete".to_owned(),
        provider_timeout_seconds: 1,
        ..AllocationHarnessOptions::default()
    })
    .await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap_or_else(|_| panic!("create first runner"));
    let mut second = harness.request.clone();
    second
        .bootstrap
        .as_mut()
        .unwrap_or_else(|| panic!("second bootstrap"))
        .name = "runner-second".to_owned();
    harness
        .service
        .handle(second)
        .await
        .unwrap_or_else(|_| panic!("create second runner"));
    harness
        .journal
        .request_allocation_cleanup("controller-1", Some("pool-1"))
        .unwrap_or_else(|_| panic!("persist cleanup intents"));
    drop(
        harness
            .service
            .0
            .take()
            .unwrap_or_else(|| panic!("running allocation service")),
    );
    drop(
        harness
            .journal
            .0
            .take()
            .unwrap_or_else(|| panic!("running journal")),
    );
    let reopened = Arc::new(harness._fixture.journal());
    let service = Box::pin(rebuild_allocation_service(
        &harness._fixture,
        reopened.clone(),
        &harness.aws_endpoint,
        &harness.old_config,
        &harness.current_config,
        "slow_first_delete",
    ))
    .await;
    harness.journal.0 = Some(reopened.clone());
    harness.service.0 = Some(service);
    harness
        .journal
        .recover_startup_allocations()
        .unwrap_or_else(|_| panic!("recover durable cleanup intents"));

    let ready_path = PathBuf::from(format!("{}.second-ready", harness.lifecycle_log.display()));
    let started = tokio::time::Instant::now();
    let recovery = harness.service.recover_pending_cleanup(2);
    let reveal_absence = release_fast_delete_after_native_absence(&harness, &ready_path);
    let (result, ()) = tokio::join!(recovery, reveal_absence);
    assert!(matches!(result, Err(AllocationError::Ambiguous)));
    assert!(
        started.elapsed() < Duration::from_millis(1800),
        "slow cleanup must share one deadline with independent child work"
    );
    assert_restart_cleanup_effects(&harness);
}

/// Exercise interruption decisions through the production recovery service.
/// Verifies that verified interruptions without native presence do not create cleanup work.
///
/// # Panics
/// Panics when the local allocation fixture fails an assertion or setup step.
pub async fn verified_interruption_only_case() {
    spot_observation_case(None, 1).await;
    spot_observation_case(Some(0), 0).await;
    spot_observation_case(Some(900), 0).await;
    first_delete_retry_excludes_interruption().await;
    interruption_poll_progress_is_independent().await;
    spot_conflicting_source_case().await;
}
