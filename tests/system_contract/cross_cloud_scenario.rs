use super::*;

pub(super) struct CrossCloudScenario {
    pub(super) fixture: JournalFixture,
    pub(super) journal: Option<Arc<outrider::journal::Journal>>,
    pub(super) config: ConfigPatch,
    pub(super) aws: SystemAwsServer,
    pub(super) azure: CloudHttpFixture,
    pub(super) gcp: CloudHttpFixture,
    pub(super) dependencies: DependencyFixture,
    pub(super) config_path: PathBuf,
    pub(super) address: String,
    pub(super) lifecycle_logs: BTreeMap<String, PathBuf>,
    pub(super) server: JoinHandle<io::Result<()>>,
    pub(super) recovery: JoinHandle<()>,
}

impl CrossCloudScenario {
    pub(super) async fn start() -> Result<Self, Box<dyn std::error::Error>> {
        let fixture = JournalFixture::new();
        let lifecycle_logs = BTreeMap::from([
            (
                "aws".to_owned(),
                fixture.directory.join("aws-lifecycle.jsonl"),
            ),
            (
                "azure".to_owned(),
                fixture.directory.join("azure-lifecycle.jsonl"),
            ),
            (
                "gcp".to_owned(),
                fixture.directory.join("gcp-lifecycle.jsonl"),
            ),
        ]);
        let aws_log = lifecycle_logs.get("aws").ok_or("AWS log missing")?.clone();
        let aws = system_aws_server(
            true,
            NativeOwnership {
                controller_id: CONTROLLER_ID.to_owned(),
                pool_id: "pool-system-201".to_owned(),
                lifecycle_log: Arc::new(Mutex::new(Some(aws_log))),
                slow_next_request: None,
            },
        )
        .await;
        aws.native_inspection_mode.store(1, Ordering::Relaxed);
        let azure = cloud_http_fixture(
            CloudApi::Azure,
            lifecycle_logs
                .get("azure")
                .ok_or("Azure log missing")?
                .clone(),
        )
        .await?;
        let gcp = cloud_http_fixture(
            CloudApi::Gcp,
            lifecycle_logs.get("gcp").ok_or("GCP log missing")?.clone(),
        )
        .await?;
        let database = fixture.directory.join("journal.sqlite3");
        let dependencies = DependencyFixture::start(database.clone()).await?;

        let mut aws_binding = allocation_binding(&fixture, "normal");
        aws_binding.executable_path = system_child_executable(
            &fixture,
            "aws",
            "i-synthetic-001",
            lifecycle_logs.get("aws").ok_or("AWS log missing")?,
        )?;
        aws_binding.executable_sha256 = sha256_file(&aws_binding.executable_path)?;
        aws_binding.inherited_environment = system_child_environment();
        let azure_configuration = "location = \"eastus\"\n[credentials]\nsubscription_id = \"11111111-1111-4111-8111-111111111111\"\n[credentials.service_principal]\ntenant_id = \"22222222-2222-4222-8222-222222222222\"\nclient_id = \"33333333-3333-4333-8333-333333333333\"\nclient_secret = \"synthetic-secret\"\n";
        let azure_config_path = fixture.directory.join("azure-provider.toml");
        fs::write(&azure_config_path, azure_configuration)?;
        let azure_binding = system_provider_binding(
            &fixture,
            "azure-child",
            "azure",
            "11111111-1111-4111-8111-111111111111",
            "eastus",
            &azure_config_path,
            "runner-azure-system",
        )?;
        let gcp_credentials = fixture.directory.join("gcp-credentials.json");
        fs::write(
            &gcp_credentials,
            r#"{"type":"service_account","client_email":"fixture@example.invalid","private_key":"fixture"}"#,
        )?;
        let gcp_configuration = format!(
            "project_id = \"system-project\"\nzone = \"us-central1-a\"\ncredentials_file = \"{}\"\nnetwork_id = \"projects/system-project/global/networks/runners\"\nsubnetwork_id = \"projects/system-project/regions/us-central1/subnetworks/runners\"\nexternal_ip_access = false\nenable_regional_placement = false\n",
            gcp_credentials.display()
        );
        let gcp_config_path = fixture.directory.join("gcp-provider.toml");
        fs::write(&gcp_config_path, gcp_configuration)?;
        let gcp_binding = system_provider_binding(
            &fixture,
            "gcp-child",
            "gcp",
            "system-project",
            "us-central1-a",
            &gcp_config_path,
            "runner-gcp-system",
        )?;

        let mut config = allocation_central(&aws_binding, "newAMI");
        config
            .aws_catalog_bindings
            .as_mut()
            .ok_or("AWS catalog missing")?
            .truncate(1);
        let aws_network = config
            .aws_catalog_bindings
            .as_mut()
            .and_then(|catalogs| catalogs.first_mut())
            .and_then(|catalog| catalog.locations.remove("us-east-1"))
            .ok_or("AWS location alias missing")?;
        config
            .aws_catalog_bindings
            .as_mut()
            .and_then(|catalogs| catalogs.first_mut())
            .ok_or("AWS catalog missing after alias update")?
            .locations
            .insert("east-ci".to_owned(), aws_network);
        let azure_catalog: AzureCatalogBinding = serde_json::from_value(json!({
            "id":"azure-east","provider_binding_id":"azure-child","account_id":"11111111-1111-4111-8111-111111111111",
            "subscription_id":"11111111-1111-4111-8111-111111111111","pricing_location":"eastus",
            "credential_reference":"provider-binding:azure-child:configuration",
            "locations":{"east-ci":{"location":"eastus","vnet_subnet_id":null,"public_ip_sku":null}},
            "images":{"ubuntu-24":{"image_id":"Canonical:UbuntuServer:22_04-lts-gen2:22.04.202601010","minimum_os_disk_gib":30,
                "minimum_os_disk_source":"operator verified image metadata","capabilities":["docker","cloud-init"],
                "licensing":"no_additional_software_charge","licensing_source":"operator verified image licensing"}},
            "storage_classes":{"gp3":"Premium_LRS"},"accelerator_facts":{}
        }))?;
        let gcp_catalog: GcpCatalogBinding = serde_json::from_value(json!({
            "id":"gcp-zonal","provider_binding_id":"gcp-child","project_id":"system-project","zone":"us-central1-a",
            "pricing_location":"us-central1","credential_reference":"provider-binding:gcp-child:configuration",
            "images":{"ubuntu-24":{"image_id":"projects/system-project/global/images/ubuntu-24","os":"linux",
                "os_source":"system fixture image metadata","architecture":"amd64","capabilities":["docker","cloud-init"],
                "capabilities_source":"system fixture image metadata","license_free_source":"operator verified image licensing","license_facts":{}}},
            "storage_classes":{"gp3":"pd-balanced"},"storage_billing_skus":{"gp3":"services/6F81-5844-456A/skus/PD-BALANCED"},
            "external_ip_billing_sku":null,"accelerator_facts":{},
            "machine_facts":{"n2-standard-8":{"shared_cpu_billing_sku":null,"bundled_local_ssd_gib":null,"bundled_local_ssd_partitions":null,"local_ssd_billing_sku":null,"local_ssd_billing_quantity":null,"max_pd_iops":10000,"max_pd_throughput_mib_s":500,"source":"operator verified machine limits"}},
            "disk_performance":{"gp3":{"base_iops":3000,"iops_per_gib":6,"maximum_iops":80000,"base_throughput_mib_s":140,"throughput_per_gib_mib_s":0.3,"maximum_throughput_mib_s":1200,"source":"operator verified disk performance"}},
            "capacity_advice_enabled":false
        }))?;
        config.provider_bindings = Some(vec![
            aws_binding.clone(),
            azure_binding.clone(),
            gcp_binding.clone(),
        ]);
        config.azure_catalog_bindings = Some(vec![azure_catalog.clone()]);
        config.gcp_catalog_bindings = Some(vec![gcp_catalog.clone()]);
        configure_cross_cloud_limits(&mut config)?;
        configure_cross_cloud_profiles(&mut config)?;
        for profile in ["linux-large", "linux-gcp", "linux-azure"] {
            config
                .normalize_profile(None, profile, None)
                .map_err(|error| {
                    format!("cross-cloud profile {profile} is not administrator eligible: {error}")
                })?;
        }
        let ingress = config.ingress.as_mut().ok_or("ingress settings missing")?;
        ingress.github_api_origin = dependencies.origin.clone();
        ingress.github_token_file = fixture.directory.join("github.token").display().to_string();
        ingress.garm_api_origin = dependencies.origin.clone();
        ingress.garm_token_file = fixture.directory.join("garm.token").display().to_string();
        ingress.controller_id = CONTROLLER_ID.to_owned();
        ingress.repositories = [(
            "101".to_owned(),
            IngressRepository {
                garm_repository_id: REPOSITORY_ID.to_owned(),
                webhook_secret_file: fixture
                    .directory
                    .join("webhook.secret")
                    .display()
                    .to_string(),
                allow_forks: false,
                template_id: None,
            },
        )]
        .into_iter()
        .collect();
        config.reruns = Some(RerunSettings {
            enabled: false,
            max_per_run: 1,
            actions_write_permission: false,
        });
        fs::write(fixture.directory.join("daemon.token"), DAEMON_TOKEN)?;
        fs::write(fixture.directory.join("github.token"), GITHUB_TOKEN)?;
        fs::write(fixture.directory.join("garm.token"), GARM_TOKEN)?;
        fs::write(fixture.directory.join("webhook.secret"), WEBHOOK_SECRET)?;
        let address = available_address()?;
        let control = config.control.as_mut().ok_or("control settings missing")?;
        control.listen_address = address.clone();
        control.database_path = database.display().to_string();
        control.auth_token_file = fixture.directory.join("daemon.token").display().to_string();
        control.provider_timeout_seconds = 5;
        let config_path = fixture.directory.join("cross-cloud.toml");
        let mut config_value = serde_json::to_value(&config)?;
        strip_null_configuration_fields(&mut config_value);
        fs::write(&config_path, serialize_native_central(&config_value)?)?;

        let journal = Arc::new(fixture.journal());
        let services = cross_cloud_services(
            config.clone(),
            &fixture,
            journal.clone(),
            &aws.endpoint,
            &azure.endpoint,
            &gcp.endpoint,
        )
        .await?;
        let allocation = services.allocation;
        let ingress_service = services.ingress;
        let recovery_service = Arc::new(RecoveryService::new(
            allocation.clone(),
            ingress_service.clone(),
            journal.clone(),
        ));
        recovery_service.startup()?;
        let recovery_loop = recovery_service.clone();
        let recovery = tokio::spawn(async move {
            loop {
                if recovery_loop.iteration(32).await.is_err() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let app = outrider::daemon::build_router(
            DAEMON_TOKEN.as_bytes().to_vec(),
            allocation,
            ingress_service,
        )?;
        let listener = TokioTcpListener::bind(&address).await?;
        let server =
            tokio::spawn(async move { axum::serve(listener, app).await.map_err(io::Error::other) });
        Ok(Self {
            fixture,
            journal: Some(journal),
            config,
            aws,
            azure,
            gcp,
            dependencies,
            config_path,
            address,
            lifecycle_logs,
            server,
            recovery,
        })
    }

    pub(super) async fn restart_with_config(
        &mut self,
        config: ConfigPatch,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.server.abort();
        if !(&mut self.server)
            .await
            .is_err_and(|error| error.is_cancelled())
        {
            return Err("cross-cloud daemon stopped before restart".into());
        }
        self.recovery.abort();
        if !(&mut self.recovery)
            .await
            .is_err_and(|error| error.is_cancelled())
        {
            return Err("cross-cloud recovery task stopped before restart".into());
        }
        let mut config_value = serde_json::to_value(&config)?;
        strip_null_configuration_fields(&mut config_value);
        fs::write(&self.config_path, serialize_native_central(&config_value)?)?;

        let journal = self
            .journal
            .as_ref()
            .ok_or("cross-cloud journal is closed")?
            .clone();
        let services = cross_cloud_services(
            config.clone(),
            &self.fixture,
            journal.clone(),
            &self.aws.endpoint,
            &self.azure.endpoint,
            &self.gcp.endpoint,
        )
        .await?;
        let recovery_service = Arc::new(RecoveryService::new(
            services.allocation.clone(),
            services.ingress.clone(),
            journal,
        ));
        recovery_service.startup()?;
        let recovery_loop = recovery_service.clone();
        self.recovery = tokio::spawn(async move {
            loop {
                if recovery_loop.iteration(32).await.is_err() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let app = outrider::daemon::build_router(
            DAEMON_TOKEN.as_bytes().to_vec(),
            services.allocation,
            services.ingress,
        )?;
        let listener = TokioTcpListener::bind(&self.address).await?;
        self.server =
            tokio::spawn(async move { axum::serve(listener, app).await.map_err(io::Error::other) });
        self.config = config;
        Ok(())
    }

    pub(super) async fn send_signed_event(
        &self,
        label: &str,
        delivery: u16,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let job_id = self.dependencies.set_job_label(label)?;
        let mut body: Value =
            serde_json::from_slice(include_bytes!("../fixtures/system/workflow-job.json"))?;
        body["workflow_job"]["id"] = json!(job_id);
        body["workflow_job"]["labels"] = json!([label]);
        let body = serde_json::to_vec(&body)?;
        let mut mac = HookMac::new_from_slice(WEBHOOK_SECRET)?;
        mac.update(&body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/webhooks/github", self.address))
            .header("x-github-event", "workflow_job")
            .header(
                "x-github-delivery",
                format!("00000000-0000-4000-8000-{delivery:012}"),
            )
            .header("x-hub-signature-256", signature)
            .body(body)
            .send()
            .await?;
        if response.status() != StatusCode::ACCEPTED {
            return Err(format!("signed webhook intake returned {}", response.status()).into());
        }
        self.dependencies
            .wait_forwarded()
            .await
            .map_err(|error| {
                let requests = self.dependencies.requests().iter().map(|request| {
                    if request.path.ends_with("/check-runs") {
                        format!("{} {}", request.path, String::from_utf8_lossy(&request.body))
                    } else {
                        request.path.clone()
                    }
                }).collect::<Vec<_>>();
                let observations = self.dependencies.snapshot_observations();
                format!("signed webhook did not forward for label {label}: {error}; dependency paths={requests:?}; snapshots={observations:?}")
            })?;
        let snapshot = self
            .journal
            .as_ref()
            .ok_or("cross-cloud journal is closed")?
            .pool_snapshot(101, label)?
            .ok_or("signed snapshot missing")?;
        let document: outrider::github::SnapshotDocument =
            serde_json::from_str(&snapshot.config_json)?;
        let expected_pool_id = if label.ends_with(":linux-gcp") {
            "pool-system-202"
        } else if label.ends_with(":linux-azure") {
            "pool-system-203"
        } else {
            "pool-system-201"
        };
        if snapshot.state != "enabled" || snapshot.garm_pool_id.as_deref() != Some(expected_pool_id)
        {
            return Err(
                format!("signed snapshot was not enabled and registered: {snapshot:?}").into(),
            );
        }
        if snapshot.request_token != label {
            return Err("signed snapshot did not retain the exact request token".into());
        }
        let expected_accounts = if label.ends_with(":linux-gcp") {
            vec!["system-project".to_owned()]
        } else if label.ends_with(":linux-azure") {
            vec!["11111111-1111-4111-8111-111111111111".to_owned()]
        } else {
            vec![
                "synthetic-account".to_owned(),
                "11111111-1111-4111-8111-111111111111".to_owned(),
                "system-project".to_owned(),
            ]
        };
        if document.resolved_profile.accounts != expected_accounts {
            return Err(format!(
                "signed event did not normalize the expected provider scope: {:?}",
                document.resolved_profile.accounts
            )
            .into());
        }
        Ok(snapshot.id)
    }

    pub(super) async fn create_runner(
        &self,
        snapshot_id: &str,
        name: &str,
    ) -> Result<Output, Box<dyn std::error::Error>> {
        let mut bootstrap: Value =
            serde_json::from_slice(include_bytes!("../fixtures/system/bootstrap.json"))?;
        bootstrap["name"] = json!(name);
        let pool_id = self
            .dependencies
            .state
            .pool
            .lock()
            .map_err(|_| "GARM fixture pool lock poisoned")?
            .as_ref()
            .and_then(|pool| pool["id"].as_str())
            .ok_or("GARM did not create a pool")?
            .to_owned();
        bootstrap["pool_id"] = json!(pool_id);
        bootstrap["extra_specs"] = json!({"snapshot_id":snapshot_id});
        Ok(provider_process(
            &self.config_path,
            "CreateInstance",
            Some(&pool_id),
            Some(&serde_json::to_vec(&bootstrap)?),
        )
        .await?)
    }

    pub(super) async fn remove_all(&self) -> Result<Output, Box<dyn std::error::Error>> {
        Ok(provider_process(&self.config_path, "RemoveAllInstances", None, None).await?)
    }

    pub(super) async fn restart_services(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let old_server = std::mem::replace(&mut self.server, tokio::spawn(async { Ok(()) }));
        old_server.abort();
        if !old_server.await.is_err_and(|error| error.is_cancelled()) {
            return Err("cross-cloud router stopped before restart".into());
        }
        let old_recovery = std::mem::replace(&mut self.recovery, tokio::spawn(async {}));
        old_recovery.abort();
        if !old_recovery.await.is_err_and(|error| error.is_cancelled()) {
            return Err("cross-cloud recovery stopped before restart".into());
        }

        let old_journal = self
            .journal
            .take()
            .ok_or("cross-cloud journal was already closed")?;
        drop(old_journal);
        let journal = Arc::new(self.fixture.journal());
        let services = cross_cloud_services(
            self.config.clone(),
            &self.fixture,
            journal.clone(),
            &self.aws.endpoint,
            &self.azure.endpoint,
            &self.gcp.endpoint,
        )
        .await?;
        let recovery_service = Arc::new(RecoveryService::new(
            services.allocation.clone(),
            services.ingress.clone(),
            journal.clone(),
        ));
        recovery_service.startup()?;
        let recovery_loop = recovery_service.clone();
        self.recovery = tokio::spawn(async move {
            loop {
                if recovery_loop.iteration(32).await.is_err() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let app = outrider::daemon::build_router(
            DAEMON_TOKEN.as_bytes().to_vec(),
            services.allocation,
            services.ingress,
        )?;
        let listener = TokioTcpListener::bind(&self.address).await?;
        self.server =
            tokio::spawn(async move { axum::serve(listener, app).await.map_err(io::Error::other) });
        self.journal = Some(journal);
        for _ in 0..100 {
            if daemon_ready(&self.address).await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err("restarted cross-cloud router did not become ready".into())
    }
}

impl Drop for CrossCloudScenario {
    fn drop(&mut self) {
        self.aws.task.abort();
        self.server.abort();
        self.recovery.abort();
    }
}
