use super::*;

pub(super) const AZURE_SUBSCRIPTION: &str = "11111111-1111-4111-8111-111111111111";
pub(super) const AZURE_CHILD_CONFIG: &str = "location = \"eastus\"\n[credentials]\nsubscription_id = \"11111111-1111-4111-8111-111111111111\"\n[credentials.service_principal]\ntenant_id = \"22222222-2222-4222-8222-222222222222\"\nclient_id = \"33333333-3333-4333-8333-333333333333\"\nclient_secret = \"synthetic-secret\"\n";

#[derive(Debug)]
pub(super) struct AzureAllocationCredential;

pub(super) struct AzureAllocationServer {
    pub(super) endpoint: String,
    pub(super) native_mode: Arc<AtomicU64>,
    pub(super) task: tokio::task::JoinHandle<std::io::Result<()>>,
}

pub(super) struct AzureCleanupHarness {
    pub(super) base: AllocationHarness,
    pub(super) service: AllocationService,
    pub(super) server: AzureAllocationServer,
}

impl AzureCleanupHarness {
    async fn new(child_mode: &str) -> Self {
        let mut base = AllocationHarness::new(AllocationHarnessOptions::default()).await;
        let mut binding = allocation_binding(&base._fixture, "normal");
        binding.id = "azure-child".to_owned();
        binding.cloud = "azure".to_owned();
        binding.account = "build-account".to_owned();
        binding.location = "eastus".to_owned();
        fs::write(&binding.configuration_path, AZURE_CHILD_CONFIG)
            .unwrap_or_else(|error| panic!("allocation fixture: {error}"));
        binding.configuration_sha256 = hex::encode(Sha256::digest(AZURE_CHILD_CONFIG.as_bytes()));
        let mut config = allocation_central(&binding, "oldAMI");
        config
            .control
            .as_mut()
            .unwrap_or_else(|| panic!("Azure allocation control"))
            .provider_timeout_seconds = 5;
        let ingress = config
            .ingress
            .as_mut()
            .unwrap_or_else(|| panic!("Azure ingress"));
        ingress.allocation_timeout_seconds = 6;
        ingress.timeout_margin_seconds = 1;
        config.provider_bindings = Some(vec![binding.clone()]);
        config.aws_catalog_bindings = None;
        let catalog: outrider::config::AzureCatalogBinding = serde_json::from_value(json!({
            "id":"azure-east","provider_binding_id":"azure-child","account_id":"build-account",
            "subscription_id":AZURE_SUBSCRIPTION,"pricing_location":"eastus",
            "credential_reference":"provider-binding:azure-child:configuration",
            "locations":{"eastus":{"location":"eastus","vnet_subnet_id":null,"public_ip_sku":null}},
            "images":{"ubuntu-24":{"image_id":"Canonical:UbuntuServer:22_04-lts-gen2:22.04.202601010","minimum_os_disk_gib":30,
                "minimum_os_disk_source":"operator-verified image","capabilities":["docker","cloud-init"],
                "licensing":"no_additional_software_charge","licensing_source":"operator-verified image licensing"}},
            "storage_classes":{"standard":"Premium_LRS"},"accelerator_facts":{}
        })).unwrap_or_else(|error| panic!("allocation fixture: {error}"));
        config.azure_catalog_bindings = Some(vec![catalog.clone()]);
        let limits = config
            .limits
            .as_mut()
            .unwrap_or_else(|| panic!("Azure allocation limits"));
        limits.allowed_clouds.insert("azure".to_owned());
        limits.allowed_accounts.insert("build-account".to_owned());
        limits.allowed_locations.insert("eastus".to_owned());
        limits.allowed_machine_types.insert(
            "azure".to_owned(),
            BTreeSet::from(["Standard_D2s_v3".to_owned()]),
        );
        let mut document = snapshot_document(config.clone(), "azure-route-token");
        document.resolved_profile.accounts = vec!["build-account".to_owned()];
        document.resolved_profile.locations = vec!["eastus".to_owned()];
        document.resolved_profile.machine_types =
            BTreeMap::from([("azure".to_owned(), vec!["Standard_D2s_v3".to_owned()])]);
        document.resolved_profile.boot_disk.class = "standard".to_owned();
        base.journal
            .save_pool_snapshot(&PoolSnapshot {
                id: "azure-snapshot-id".to_owned(),
                repository_id: 7,
                request_token: "azure-route-token".to_owned(),
                config_json: serde_json::to_string(&document)
                    .unwrap_or_else(|error| panic!("allocation fixture: {error}")),
                state: "enabled".to_owned(),
                garm_pool_id: Some("pool-1".to_owned()),
                expires_at: unix_now_for_test() + 3600,
            })
            .unwrap_or_else(|error| panic!("allocation fixture: {error}"));
        base.request.pool_extra_specs = Some(json!({"snapshot_id":"azure-snapshot-id"}));
        base.request
            .bootstrap
            .as_mut()
            .unwrap_or_else(|| panic!("Azure allocation bootstrap"))
            .labels[0] = "azure-route-token".to_owned();
        let server = azure_allocation_server().await;
        let mut settings = AzureBackend::settings_from_bindings(&catalog, &binding)
            .unwrap_or_else(|error| panic!("allocation fixture: {error}"));
        settings.management_endpoint = server.endpoint.clone();
        settings.retail_endpoint = server.endpoint.clone();
        let backend = AzureBackend::with_clients(
            settings,
            reqwest::Client::new(),
            Arc::new(AzureAllocationCredential),
            tokio::time::Instant::now() + Duration::from_secs(3),
        )
        .await
        .unwrap_or_else(|error| panic!("allocation fixture: {error}"));
        let environment = BTreeMap::from([
            (
                OsString::from("OUTRIDER_FIXTURE_MODE"),
                OsString::from(child_mode),
            ),
            (
                OsString::from("OUTRIDER_LIFECYCLE_LOG"),
                base.lifecycle_log.as_os_str().to_owned(),
            ),
            (
                OsString::from("OUTRIDER_RECORD"),
                base.record_path.as_os_str().to_owned(),
            ),
            (
                OsString::from("OUTRIDER_CREATE_BARRIER"),
                base._fixture
                    .directory
                    .join("create-barrier")
                    .as_os_str()
                    .to_owned(),
            ),
            (
                OsString::from("OUTRIDER_NATIVE_LOCATOR"),
                OsString::from("runner-old-image"),
            ),
        ]);
        let registry = ProviderRegistry::new(
            vec![binding.clone()],
            environment.clone(),
            ProcessLimits::default(),
        )
        .unwrap_or_else(|error| panic!("allocation fixture: {error}"));
        let fingerprint = catalog_configuration_fingerprint(&config, "azure", "azure-east")
            .unwrap_or_else(|error| panic!("allocation fixture: {error}"));
        let service = AllocationService::new(
            config,
            base.journal.clone(),
            vec![ConnectedCatalog::azure(
                backend,
                fingerprint,
                "azure-east".to_owned(),
                binding.id,
                AZURE_SUBSCRIPTION.to_owned(),
                BTreeSet::from(["eastus".to_owned()]),
            )],
            registry,
            environment,
        )
        .unwrap_or_else(|error| panic!("allocation fixture: {error}"));
        Self {
            base,
            service,
            server,
        }
    }
}

pub(super) fn azure_allocation_vm(mode: u64) -> Value {
    json!({
        "id":format!("/subscriptions/{AZURE_SUBSCRIPTION}/resourceGroups/runner-old-image/providers/Microsoft.Compute/virtualMachines/runner-old-image"),
        "name":"runner-old-image", "location":"eastus",
        "tags":{"garm-controller-id":if mode == 3 {"foreign-controller"} else {"controller-1"},"garm-pool-id":if mode == 7 {"foreign-pool"} else {"pool-1"}},
        "properties":{"vmId":if mode == 4 {"replacement-vm"} else {"original-vm"}, "instanceView":{"statuses":[{"code":"PowerState/running"}]}}
    })
}

pub(super) fn azure_allocation_meter(
    service: &str,
    product: &str,
    sku: &str,
    unit: &str,
    rate: &str,
) -> Value {
    let meter_name = if service == "Storage" {
        format!("{sku} Disk")
    } else {
        sku.to_owned()
    };
    json!({
        "serviceName":service, "productName":product,"skuName":sku,"armSkuName":"Standard_D2s_v3","meterName":meter_name,
        "armRegionName":"eastus","unitOfMeasure":unit,"type":"Consumption","isPrimaryMeterRegion":true,
        "effectiveStartDate":"2026-01-01T00:00:00Z","currencyCode":"USD","tierMinimumUnits":0,
        "retailPrice":rate,"unitPrice":rate,"meterId":sku
    })
}

pub(super) fn azure_allocation_response(
    request: hyper::Request<Incoming>,
    native_mode: Arc<AtomicU64>,
) -> Result<hyper::Response<http_body_util::Full<bytes::Bytes>>, std::io::Error> {
    let path = request.uri().path();
    let mode = native_mode.load(Ordering::Relaxed);
    let mut status = 200;
    let body = if path.contains("/virtualMachines/") {
        if mode == 0 || mode == 2 {
            status = if mode == 0 { 404 } else { 503 };
            json!({"error":{"code":"ResourceNotFound","message":"synthetic read result"}})
        } else {
            azure_allocation_vm(mode)
        }
    } else if path.ends_with("/virtualMachines") {
        if mode == 6 {
            native_mode.store(1, Ordering::Relaxed);
            status = 503;
        }

        if mode == 5 {
            // The successful create precedes visibility in subscription inventory.
            native_mode.store(1, Ordering::Relaxed);
        }

        let mut vm = azure_allocation_vm(mode);
        if request
            .uri()
            .query()
            .is_some_and(|query| query.contains("statusOnly=true"))
        {
            // Azure's status-only response omits ownership metadata.
            vm.as_object_mut()
                .unwrap_or_else(|| panic!("Azure VM fixture object"))
                .remove("tags");
        } else {
            vm["properties"]
                .as_object_mut()
                .unwrap_or_else(|| panic!("Azure VM fixture properties"))
                .remove("instanceView");
        }

        json!({"value":if mode == 0 || mode == 2 || mode == 5 {vec![]} else {vec![vm]}})
    } else if path.ends_with("/skus") {
        json!({"value":[{"name":"Standard_D2s_v3","family":"Dsv3","resourceType":"virtualMachines","locations":["eastus"],"capabilities":[
            {"name":"CpuArchitectureType","value":"x64"},{"name":"PremiumIO","value":"True"},{"name":"vCPUs","value":"16"},{"name":"vCPUsAvailable","value":"8"},
            {"name":"MemoryGB","value":"16"},{"name":"MaxDataDiskCount","value":"4"},
            {"name":"UncachedDiskIOPS","value":"20000"},{"name":"UncachedDiskBytesPerSecond","value":"60000000.5"}
        ],"restrictions":[]}]})
    } else if path.contains("/versions/") {
        json!({"properties":{"architecture":"x64","osDiskImage":{"operatingSystem":"Linux"},"plan":null}})
    } else {
        json!({"Items":[
            azure_allocation_meter("Virtual Machines","Virtual Machines Dsv3 Series","D2s v3","1 Hour","0.10"),
            azure_allocation_meter("Storage","Premium SSD Managed Disks","P6 LRS","1/Month","3.00")
        ],"NextPageLink":null})
    };
    Ok(hyper::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(http_body_util::Full::new(bytes::Bytes::from(
            body.to_string(),
        )))
        .unwrap_or_else(|error| panic!("allocation fixture: {error}")))
}

pub(super) async fn azure_allocation_server() -> AzureAllocationServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("allocation fixture: {error}"));
    let endpoint = format!(
        "http://{}",
        listener
            .local_addr()
            .unwrap_or_else(|error| panic!("allocation fixture: {error}"))
    );
    let native_mode = Arc::new(AtomicU64::new(0));
    let server_mode = native_mode.clone();
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await?;
            let native_mode = server_mode.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    std::future::ready(azure_allocation_response(request, native_mode.clone()))
                });
                if let Err(error) = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                {
                    eprintln!("Azure allocation fixture connection: {error}");
                }
            });
        }
    });
    AzureAllocationServer {
        endpoint,
        native_mode,
        task,
    }
}

#[tokio::test]
async fn azure_vmless_cleanup_dispatches_reserved_name_and_keeps_uncertain_create_fence() {
    let harness = AzureCleanupHarness::new("create_failure").await;
    assert!(matches!(
        harness.service.handle(harness.base.request.clone()).await,
        Err(AllocationError::Ambiguous)
    ));
    assert_eq!(harness.base.allocation().state, "deleting");
    assert!(matches!(
        harness
            .service
            .handle(
                harness
                    .base
                    .lifecycle_request("DeleteInstance", "runner-old-image")
            )
            .await,
        Err(AllocationError::Ambiguous)
    ));
    let allocation = harness.base.allocation();
    assert!(allocation.tombstone);
    assert_eq!(allocation.state, "deleting");
    assert!(allocation.native_identity_json.is_none());
    assert_eq!(
        harness
            .base
            .journal
            .active_allocation_counts("controller-1", "pool-1")
            .unwrap(),
        (1, 1)
    );
    let action = harness
        .base
        .journal
        .allocation_action(&allocation.allocation_id)
        .unwrap()
        .unwrap();
    assert_eq!(action.state, "pending");
    assert!(action.unresolved_create);
    let connection =
        rusqlite::Connection::open(harness.base._fixture.directory.join("journal.sqlite3"))
            .expect("open Azure cleanup journal");
    connection
        .execute(
            "UPDATE allocation_actions SET next_attempt_at=CAST(strftime('%s','now') AS INTEGER)+300 WHERE allocation_id=?1 AND reusable=1",
            [&allocation.allocation_id],
        )
        .expect("keep first Azure delete not due");
    drop(connection);
    let records = fs::read_to_string(&harness.base.lifecycle_log).unwrap();
    let delete: Value = records
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["command"] == "DeleteInstance")
        .unwrap();
    assert_eq!(delete["instance_id"], "runner-old-image");
    assert_eq!(
        delete["stdin"], "",
        "qualified Azure Delete carries only the retained environment locator"
    );
    assert_eq!(delete["controller_id"], "controller-1");
    assert_eq!(delete["pool_id"], "pool-1");
    let mut remove = harness.base.lifecycle_request("RemoveAllInstances", "");
    remove.instance_id = None;
    assert!(harness.service.handle(remove).await.is_err());
    assert!(matches!(
        harness.service.recover_pending_cleanup(10).await,
        Ok(0)
    ));
    let repeated = fs::read_to_string(&harness.base.lifecycle_log).unwrap();
    assert_eq!(
        repeated
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|record| record["command"] == "DeleteInstance")
            .count(),
        1,
        "direct delete, RemoveAll, and recovery share one backoff gate"
    );
}

#[tokio::test]
async fn azure_vmless_cleanup_blocks_failed_probe_or_foreign_replacement() {
    for mode in [2, 3] {
        let harness = AzureCleanupHarness::new("create_failure").await;
        assert!(matches!(
            harness.service.handle(harness.base.request.clone()).await,
            Err(AllocationError::Ambiguous)
        ));
        harness.server.native_mode.store(mode, Ordering::Relaxed);
        assert!(
            harness
                .service
                .handle(
                    harness
                        .base
                        .lifecycle_request("DeleteInstance", "runner-old-image")
                )
                .await
                .is_err()
        );
        assert!(
            !harness
                .base
                .lifecycle_commands()
                .contains(&"DeleteInstance".to_owned())
        );
        assert!(harness.base.allocation().tombstone);
    }
}

#[tokio::test]
async fn azure_create_waits_for_native_inventory_visibility() {
    let harness = AzureCleanupHarness::new("normal").await;
    harness.server.native_mode.store(5, Ordering::Relaxed);
    harness
        .service
        .handle(harness.base.request.clone())
        .await
        .unwrap_or_else(|error| panic!("delayed Azure ownership must resolve: {error}"));
    let allocation = harness.base.allocation();
    assert_eq!(allocation.state, "created");
    assert!(!allocation.tombstone);
    assert!(allocation.native_identity_json.is_some());
    assert!(
        !harness
            .base
            .lifecycle_commands()
            .iter()
            .any(|command| command == "DeleteInstance")
    );
    assert_eq!(
        harness
            .base
            .journal
            .child_attempts(&allocation.allocation_id)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn azure_create_does_not_retry_inventory_errors() {
    let harness = AzureCleanupHarness::new("normal").await;
    harness.server.native_mode.store(6, Ordering::Relaxed);
    assert!(
        harness
            .service
            .handle(harness.base.request.clone())
            .await
            .is_err()
    );
    let allocation = harness.base.allocation();
    assert_eq!(allocation.state, "deleting");
    assert!(
        allocation
            .detail
            .as_deref()
            .is_some_and(|detail| { detail.starts_with("native identity lookup failed:") })
    );
    assert_eq!(harness.server.native_mode.load(Ordering::Relaxed), 1);
    assert_eq!(
        harness
            .base
            .journal
            .child_attempts(&allocation.allocation_id)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn azure_create_bounds_unresolved_inventory_by_original_deadline() {
    let harness = AzureCleanupHarness::new("normal").await;
    harness.server.native_mode.store(7, Ordering::Relaxed);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        harness.service.handle(harness.base.request.clone()),
    )
    .await
    .unwrap_or_else(|_| panic!("five-second provider budget must bound visibility wait"));
    assert!(result.is_err());
    let allocation = harness.base.allocation();
    assert_eq!(allocation.state, "deleting");
    assert!(allocation.tombstone);
    assert!(allocation.native_identity_json.is_none());
    assert!(allocation.detail.as_deref().is_some_and(|detail| {
        detail.contains("same_name=") && detail.ends_with("rejected=pool-tag")
    }));
    assert_eq!(
        harness
            .base
            .journal
            .child_attempts(&allocation.allocation_id)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn azure_tombstone_during_visibility_wait_cannot_become_created() {
    let harness = AzureCleanupHarness::new("normal").await;
    let cancel = async {
        loop {
            if let Some(allocation) = harness
                .base
                .journal
                .allocation("controller-1", "runner-old-image")
                .unwrap()
                && allocation.returned_at.is_some()
            {
                harness
                    .base
                    .journal
                    .schedule_allocation_cleanup(
                        &allocation.allocation_id,
                        "test terminal cancellation",
                    )
                    .unwrap();
                break;
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(harness.service.handle(harness.base.request.clone()), cancel)
    })
    .await
    .unwrap_or_else(|_| panic!("tombstone must stop visibility wait"));
    assert!(result.is_err());
    let allocation = harness.base.allocation();
    assert!(allocation.tombstone);
    assert_ne!(allocation.state, "created");
    assert!(allocation.native_identity_json.is_none());
    assert_eq!(
        harness
            .base
            .journal
            .child_attempts(&allocation.allocation_id)
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn azure_replaced_incarnation_blocks_known_allocation_mutation() {
    let harness = AzureCleanupHarness::new("normal").await;
    harness.server.native_mode.store(1, Ordering::Relaxed);
    harness
        .service
        .handle(harness.base.request.clone())
        .await
        .unwrap();
    let original = harness.base.allocation();
    let identity: Value =
        serde_json::from_str(original.native_identity_json.as_deref().unwrap()).unwrap();
    assert_eq!(identity["dependency_ids"]["vm_id"], "original-vm");
    harness.server.native_mode.store(4, Ordering::Relaxed);
    for command in [
        "GetInstance",
        "StartInstance",
        "StopInstance",
        "DeleteInstance",
    ] {
        assert!(
            harness
                .service
                .handle(harness.base.lifecycle_request(command, "runner-old-image"))
                .await
                .is_err()
        );
    }

    let commands = harness.base.lifecycle_commands();
    assert!(!commands.iter().any(|command| matches!(
        command.as_str(),
        "DeleteInstance" | "StartInstance" | "StopInstance" | "GetInstance"
    )));
    assert_eq!(
        harness.base.allocation().native_identity_json,
        original.native_identity_json
    );
}

#[async_trait]
impl TokenCredential for AzureAllocationCredential {
    async fn get_token(
        &self,
        _scopes: &[&str],
        _options: Option<TokenRequestOptions<'_>>,
    ) -> azure_core::Result<AccessToken> {
        Ok(AccessToken::new(
            "synthetic-local-token",
            OffsetDateTime::from_unix_timestamp(4_102_444_800)
                .unwrap_or_else(|error| panic!("allocation fixture: {error}")),
        ))
    }
}

impl Drop for AzureAllocationServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
