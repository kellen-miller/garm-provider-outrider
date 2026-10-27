use super::*;

pub(super) struct CrossCloudServices {
    pub(super) allocation: Arc<AllocationService>,
    pub(super) ingress: Arc<IngressService>,
}

pub(super) async fn cross_cloud_services(
    config: ConfigPatch,
    fixture: &JournalFixture,
    journal: Arc<outrider::journal::Journal>,
    aws_endpoint: &str,
    azure_endpoint: &str,
    gcp_endpoint: &str,
) -> Result<CrossCloudServices, Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let bindings = config
        .provider_bindings
        .as_deref()
        .ok_or("provider pins missing")?;
    let aws_binding = bindings
        .iter()
        .find(|binding| binding.id == "aws-child")
        .ok_or("AWS provider pin missing")?;
    let azure_binding = bindings
        .iter()
        .find(|binding| binding.id == "azure-child")
        .ok_or("Azure provider pin missing")?;
    let gcp_binding = bindings
        .iter()
        .find(|binding| binding.id == "gcp-child")
        .ok_or("GCP provider pin missing")?;
    let aws_catalog = config
        .aws_catalog_bindings
        .as_deref()
        .and_then(|catalogs| catalogs.first())
        .ok_or("AWS catalog missing")?;
    let azure_catalog = config
        .azure_catalog_bindings
        .as_deref()
        .and_then(|catalogs| catalogs.first())
        .ok_or("Azure catalog missing")?;
    let gcp_catalog = config
        .gcp_catalog_bindings
        .as_deref()
        .and_then(|catalogs| catalogs.first())
        .ok_or("GCP catalog missing")?;

    let mut aws_settings = AwsBackend::settings_from_bindings(aws_catalog, aws_binding, false)?;
    aws_settings.ec2_endpoint_url = Some(aws_endpoint.to_owned());
    aws_settings.pricing_endpoint_url = Some(aws_endpoint.to_owned());
    aws_settings.sts_endpoint_url = Some(aws_endpoint.to_owned());
    let shared = aws_config::defaults(BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            "AKIDTESTAWSCONTRACT",
            "secret-test-only-not-a-real-key",
            None,
            None,
            "system-contract",
        ))
        .load()
        .await;
    let ec2 = aws_sdk_ec2::Client::from_conf(
        aws_sdk_ec2::config::Builder::from(&shared)
            .endpoint_url(aws_endpoint)
            .build(),
    );
    let pricing = aws_sdk_pricing::Client::from_conf(
        aws_sdk_pricing::config::Builder::from(&shared)
            .region(aws_config::Region::new("us-east-1"))
            .endpoint_url(aws_endpoint)
            .build(),
    );
    let sts = aws_sdk_sts::Client::from_conf(
        aws_sdk_sts::config::Builder::from(&shared)
            .endpoint_url(aws_endpoint)
            .build(),
    );
    let aws_backend = AwsBackend::with_clients(aws_settings, ec2, pricing, sts, deadline).await?;

    let mut azure_settings = AzureBackend::settings_from_bindings(azure_catalog, azure_binding)?;
    azure_settings.management_endpoint = azure_endpoint.to_owned();
    azure_settings.retail_endpoint = azure_endpoint.to_owned();
    let azure_backend = AzureBackend::with_clients(
        azure_settings,
        reqwest::Client::new(),
        Arc::new(SystemAzureCredential),
        deadline,
    )
    .await?;

    let mut gcp_settings = GcpBackend::settings_from_bindings(gcp_catalog, gcp_binding)?;
    gcp_settings.compute_endpoint = gcp_endpoint.to_owned();
    gcp_settings.billing_endpoint = gcp_endpoint.to_owned();
    gcp_settings.purchasing_capabilities = BTreeSet::from(["on_demand".to_owned()]);
    let gcp_backend = GcpBackend::with_clients(
        gcp_settings,
        reqwest::Client::new(),
        AccessTokenCredentials::from(SystemGcpCredential),
        deadline,
    )
    .await?;

    let broad_profile = config.normalize_profile(None, "linux-large", None)?.profile;
    let _aws_broad_offer = quoted_catalog_offer(
        &aws_backend,
        "aws",
        "synthetic-account",
        "east-ci",
        broad_profile.clone(),
        deadline,
    )
    .await?;
    let _azure_broad_offer = quoted_catalog_offer(
        &azure_backend,
        "azure",
        "11111111-1111-4111-8111-111111111111",
        "east-ci",
        broad_profile.clone(),
        deadline,
    )
    .await?;
    let _gcp_broad_offer = quoted_catalog_offer(
        &gcp_backend,
        "gcp",
        "system-project",
        "us-central1",
        broad_profile,
        deadline,
    )
    .await?;
    let gcp_profile = config.normalize_profile(None, "linux-gcp", None)?.profile;
    let _gcp_constrained_offer = quoted_catalog_offer(
        &gcp_backend,
        "gcp",
        "system-project",
        "us-central1",
        gcp_profile,
        deadline,
    )
    .await?;

    let environment = system_process_environment(fixture);
    let registry = ProviderRegistry::new(
        bindings.to_vec(),
        environment.clone(),
        ProcessLimits::default(),
    )?;
    let catalogs = vec![
        ConnectedCatalog::aws(
            aws_backend,
            catalog_configuration_fingerprint(&config, "aws", &aws_catalog.id)?,
            aws_catalog.id.clone(),
            aws_binding.id.clone(),
            aws_binding.account.clone(),
            aws_binding.location.clone(),
        ),
        ConnectedCatalog::azure(
            azure_backend,
            catalog_configuration_fingerprint(&config, "azure", &azure_catalog.id)?,
            azure_catalog.id.clone(),
            azure_binding.id.clone(),
            azure_catalog.subscription_id.clone(),
            BTreeSet::from(["eastus".to_owned()]),
        ),
        ConnectedCatalog::gcp(
            gcp_backend,
            catalog_configuration_fingerprint(&config, "gcp", &gcp_catalog.id)?,
            gcp_catalog.id.clone(),
            gcp_binding.id.clone(),
            gcp_catalog.project_id.clone(),
            gcp_catalog.zone.clone(),
        ),
    ];
    Ok(CrossCloudServices {
        allocation: Arc::new(AllocationService::new(
            config.clone(),
            journal.clone(),
            catalogs,
            registry,
            environment,
        )?),
        ingress: Arc::new(IngressService::new(config, journal)?),
    })
}

pub(super) fn system_child_environment() -> Vec<String> {
    vec![
        "OUTRIDER_CREATE_BARRIER".to_owned(),
        "OUTRIDER_FIXTURE_MODE".to_owned(),
        "OUTRIDER_RECORD".to_owned(),
    ]
}

pub(super) fn system_process_environment(fixture: &JournalFixture) -> BTreeMap<OsString, OsString> {
    BTreeMap::from([
        (
            OsString::from("OUTRIDER_FIXTURE_MODE"),
            OsString::from("record"),
        ),
        (
            OsString::from("OUTRIDER_RECORD"),
            fixture.directory.join("child-record.json").into_os_string(),
        ),
        (
            OsString::from("OUTRIDER_CREATE_BARRIER"),
            fixture.directory.join("create-barrier").into_os_string(),
        ),
    ])
}

pub(super) fn sha256_file(path: &std::path::Path) -> io::Result<String> {
    Ok(hex::encode(sha2::Sha256::digest(fs::read(path)?)))
}

pub(super) fn system_child_executable(
    fixture: &JournalFixture,
    id: &str,
    native_locator: &str,
    lifecycle_log: &std::path::Path,
) -> io::Result<PathBuf> {
    let executable = fixture.directory.join(format!("{id}-provider"));
    outrider_xtask::provider_fixture::install(
        &executable,
        &outrider_xtask::provider_fixture::Configuration {
            scenario: outrider_xtask::provider_fixture::Scenario::System,
            native_locator: Some(native_locator.to_owned()),
            lifecycle_log: Some(lifecycle_log.to_path_buf()),
        },
    )?;
    Ok(executable)
}

pub(super) fn system_provider_binding(
    fixture: &JournalFixture,
    id: &str,
    cloud: &str,
    account: &str,
    location: &str,
    configuration_path: &std::path::Path,
    native_locator: &str,
) -> io::Result<ProviderBinding> {
    let lifecycle_log = fixture.directory.join(format!("{cloud}-lifecycle.jsonl"));
    let executable_path = system_child_executable(fixture, id, native_locator, &lifecycle_log)?;
    let configuration_sha256 = sha256_file(configuration_path)?;
    Ok(ProviderBinding {
        id: id.to_owned(),
        cloud: cloud.to_owned(),
        account: account.to_owned(),
        location: location.to_owned(),
        executable_sha256: sha256_file(&executable_path)?,
        executable_path,
        configuration_path: configuration_path.to_owned(),
        configuration_sha256,
        source_revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        expected_version: "fixture-build-1".to_owned(),
        inherited_environment: system_child_environment(),
        supported_os: BTreeSet::from(["linux".to_owned()]),
        supported_architectures: BTreeSet::from(["amd64".to_owned()]),
        purchasing_capabilities: BTreeSet::from([PurchasingCapability::OnDemand]),
    })
}

pub(super) fn configure_cross_cloud_limits(
    config: &mut ConfigPatch,
) -> Result<(), Box<dyn std::error::Error>> {
    let limits = config
        .limits
        .as_mut()
        .ok_or("administrator limits missing")?;
    limits.allowed_clouds =
        BTreeSet::from(["aws".to_owned(), "azure".to_owned(), "gcp".to_owned()]);
    limits.allowed_accounts = BTreeSet::from([
        "synthetic-account".to_owned(),
        "11111111-1111-4111-8111-111111111111".to_owned(),
        "system-project".to_owned(),
    ]);
    limits.allowed_locations = BTreeSet::from([
        "east-ci".to_owned(),
        "eastus".to_owned(),
        "us-central1".to_owned(),
        "us-central1-a".to_owned(),
    ]);
    limits.allowed_machine_types = BTreeMap::from([
        ("aws".to_owned(), BTreeSet::from(["m7i.xlarge".to_owned()])),
        (
            "azure".to_owned(),
            BTreeSet::from(["Standard_D8s_v5".to_owned()]),
        ),
        (
            "gcp".to_owned(),
            BTreeSet::from(["n2-standard-8".to_owned()]),
        ),
    ]);
    limits.allowed_machine_families = BTreeMap::from([
        ("aws".to_owned(), BTreeSet::from(["m7i".to_owned()])),
        ("azure".to_owned(), BTreeSet::from(["Dsv5".to_owned()])),
        ("gcp".to_owned(), BTreeSet::from(["n2".to_owned()])),
    ]);
    Ok(())
}

pub(super) fn configure_cross_cloud_profiles(
    config: &mut ConfigPatch,
) -> Result<(), Box<dyn std::error::Error>> {
    let profiles = config.profiles.as_mut().ok_or("profiles missing")?;
    profiles.insert("linux-large".to_owned(), serde_json::from_value(json!({
        "vcpu_min":8,"vcpu_max":8,"memory_gib_min":16,"memory_gib_max":32,
        "machine_types":{"aws":["m7i.xlarge"],"azure":["Standard_D8s_v5"],"gcp":["n2-standard-8"]},
        "machine_families":{"aws":["m7i"],"azure":["Dsv5"],"gcp":["n2"]},
        "accounts":["synthetic-account","11111111-1111-4111-8111-111111111111","system-project"],"locations":["east-ci","eastus","us-central1"],
        "boot_disk":{"class":"gp3","size_gib":40},"capabilities":["docker"]
    }))?);
    profiles.insert("linux-gcp".to_owned(), serde_json::from_value(json!({
        "vcpu_min":8,"vcpu_max":8,"memory_gib_min":16,"memory_gib_max":32,
        "machine_types":{"aws":[],"azure":[],"gcp":["n2-standard-8"]},"accounts":["system-project"],"locations":["us-central1"],
        "machine_families":{"aws":[],"azure":[],"gcp":["n2"]},
        "boot_disk":{"class":"gp3","size_gib":40},"capabilities":["docker"]
    }))?);
    profiles.insert("linux-azure".to_owned(), serde_json::from_value(json!({
        "vcpu_min":8,"vcpu_max":8,"memory_gib_min":16,"memory_gib_max":32,
        "machine_types":{"aws":[],"azure":["Standard_D8s_v5"],"gcp":[]},"accounts":["11111111-1111-4111-8111-111111111111"],"locations":["east-ci"],
        "machine_families":{"aws":[],"azure":["Dsv5"],"gcp":[]},
        "boot_disk":{"class":"gp3","size_gib":40},"capabilities":["docker"]
    }))?);
    Ok(())
}
