use super::*;

pub(super) struct SystemAwsServer {
    pub(super) endpoint: String,
    pub(super) compute_quote_requests: Arc<AtomicU64>,
    pub(super) native_inspection_mode: Arc<AtomicU64>,
    pub(super) slow_next_request: Arc<AtomicBool>,
    pub(super) native_visible: Arc<AtomicBool>,
    pub(super) request_trace: Arc<Mutex<Vec<String>>>,
    pub(super) task: tokio::task::JoinHandle<std::io::Result<()>>,
}

pub(super) async fn system_aws_server(
    initial_native_visible: bool,
    native_ownership: NativeOwnership,
) -> SystemAwsServer {
    let slow_next_request = Arc::new(AtomicBool::new(false));
    let mut native_ownership = native_ownership;
    native_ownership.slow_next_request = Some(slow_next_request.clone());
    let AwsServer {
        endpoint,
        compute_quote_requests,
        native_visible,
        collision_native_observations,
        native_inspection_mode,
        native_inspections,
        native_inspection_delay_ms,
        delayed_native_inspections,
        quote_outage,
        spot_event_time,
        spot_conflict,
        spot_slow_first,
        image_requests,
        delayed_image_requests,
        image_delay_on_request,
        image_request_delay_ms,
        request_trace,
        task,
    } = aws_server(
        initial_native_visible,
        false,
        Some(native_ownership),
        aws_sdk_fixture::SubnetMetadataBehavior::Stable,
    )
    .await;
    drop((
        image_requests,
        delayed_image_requests,
        image_delay_on_request,
        image_request_delay_ms,
    ));
    drop((
        collision_native_observations,
        native_inspections,
        native_inspection_delay_ms,
        delayed_native_inspections,
        quote_outage,
        spot_event_time,
        spot_conflict,
        spot_slow_first,
    ));
    SystemAwsServer {
        endpoint,
        compute_quote_requests,
        native_inspection_mode,
        slow_next_request,
        native_visible,
        request_trace,
        task,
    }
}

pub(super) struct SystemScenario {
    pub(super) fixture: JournalFixture,
    pub(super) aws: SystemAwsServer,
    pub(super) dependencies: DependencyFixture,
    pub(super) daemon: Child,
    pub(super) config_path: PathBuf,
    pub(super) address: String,
    pub(super) lifecycle_log: PathBuf,
    pub(super) routing_label: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ExplainMutationCounts {
    pub(super) allocations: i64,
    pub(super) actions: i64,
    pub(super) snapshots: i64,
    pub(super) webhook_deliveries: i64,
    pub(super) outbox: i64,
    pub(super) terminal_runs: i64,
    pub(super) rerun_intents: i64,
}

pub(super) fn explain_mutation_counts(
    path: &std::path::Path,
) -> rusqlite::Result<ExplainMutationCounts> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let allocations =
        connection.query_row("SELECT COUNT(*) FROM allocations", [], |row| row.get(0))?;
    let actions = connection.query_row("SELECT COUNT(*) FROM allocation_actions", [], |row| {
        row.get(0)
    })?;
    let snapshots =
        connection.query_row("SELECT COUNT(*) FROM pool_snapshots", [], |row| row.get(0))?;
    let webhook_deliveries =
        connection.query_row("SELECT COUNT(*) FROM webhook_deliveries", [], |row| {
            row.get(0)
        })?;
    let outbox =
        connection.query_row("SELECT COUNT(*) FROM ingress_outbox", [], |row| row.get(0))?;
    let terminal_runs =
        connection.query_row("SELECT COUNT(*) FROM terminal_runs", [], |row| row.get(0))?;
    let rerun_intents =
        connection.query_row("SELECT COUNT(*) FROM rerun_intents", [], |row| row.get(0))?;
    Ok(ExplainMutationCounts {
        allocations,
        actions,
        snapshots,
        webhook_deliveries,
        outbox,
        terminal_runs,
        rerun_intents,
    })
}

impl SystemScenario {
    pub(super) async fn start() -> Result<Self, Box<dyn std::error::Error>> {
        Self::start_with_macos_profile(false).await
    }

    pub(super) async fn start_with_macos_profile(
        unsupported_macos_profile: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::start_with_provider_timeout(unsupported_macos_profile, 5).await
    }

    pub(super) async fn start_with_provider_timeout(
        unsupported_macos_profile: bool,
        provider_timeout_seconds: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let lifecycle_path = Arc::new(Mutex::new(None));
        let fixture = JournalFixture::new();
        let binding = allocation_binding(&fixture, "normal");
        let mut config = allocation_central(&binding, "newAMI");
        if unsupported_macos_profile {
            let limits = config
                .limits
                .as_mut()
                .ok_or("administrator limits missing")?;
            limits.allowed_os.insert("macos".to_owned());
            limits.allowed_architectures.insert("arm64".to_owned());
            let profile = config
                .profiles
                .as_mut()
                .and_then(|profiles| profiles.get_mut("linux-large"))
                .ok_or("Linux-large profile missing")?;
            profile.os = Some("macos".to_owned());
            profile.architecture = Some("arm64".to_owned());
        }
        let aws = system_aws_server(
            true,
            NativeOwnership {
                controller_id: CONTROLLER_ID.to_owned(),
                pool_id: "pool-system-201".to_owned(),
                lifecycle_log: lifecycle_path.clone(),
                slow_next_request: None,
            },
        )
        .await;
        let database = fixture.directory.join("journal.sqlite3");
        let lifecycle_log = fixture.directory.join("system-lifecycle.jsonl");
        if let Ok(mut path) = lifecycle_path.lock() {
            *path = Some(lifecycle_log.clone());
        }
        aws.native_inspection_mode.store(1, Ordering::Relaxed);
        let dependencies = DependencyFixture::start(database.clone()).await?;
        let directory = &fixture.directory;
        let token_path = directory.join("daemon.token");
        let github_token_path = directory.join("github.token");
        let garm_token_path = directory.join("garm.token");
        let webhook_secret_path = directory.join("webhook.secret");
        fs::write(&token_path, DAEMON_TOKEN)?;
        fs::write(&github_token_path, GITHUB_TOKEN)?;
        fs::write(&garm_token_path, GARM_TOKEN)?;
        fs::write(&webhook_secret_path, WEBHOOK_SECRET)?;
        let ingress = config
            .ingress
            .as_mut()
            .ok_or("allocation ingress config missing")?;
        ingress.controller_id = CONTROLLER_ID.to_owned();
        ingress.github_api_origin = dependencies.origin.clone();
        ingress.github_token_file = github_token_path.display().to_string();
        ingress.garm_api_origin = dependencies.origin.clone();
        ingress.garm_token_file = garm_token_path.display().to_string();
        ingress.repositories = [(
            "101".to_owned(),
            IngressRepository {
                garm_repository_id: REPOSITORY_ID.to_owned(),
                webhook_secret_file: webhook_secret_path.display().to_string(),
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
        let address = available_address()?;
        let control = config
            .control
            .as_mut()
            .ok_or("allocation control config missing")?;
        control.listen_address = address.clone();
        control.database_path = database.display().to_string();
        control.auth_token_file = token_path.display().to_string();
        control.provider_timeout_seconds = provider_timeout_seconds;
        let config_path = directory.join("system.toml");
        let mut config_value = serde_json::to_value(&config)?;
        strip_null_configuration_fields(&mut config_value);
        fs::write(&config_path, serialize_native_central(&config_value)?)?;
        let daemon = start_daemon(&config_path, &address, &aws.endpoint, &lifecycle_log).await?;
        Ok(Self {
            fixture,
            aws,
            dependencies,
            daemon,
            config_path,
            address,
            lifecycle_log,
            routing_label: ROUTING_LABEL.to_owned(),
        })
    }

    pub(super) async fn send_signed_event(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.post_signed_event().await?;
        self.dependencies.wait_forwarded().await?;
        let observations = self.dependencies.snapshot_observations();
        if observations.len() != 2
            || observations[0].checkpoint != SnapshotCheckpoint::PoolEnable
            || observations[1].checkpoint != SnapshotCheckpoint::Forward
        {
            return Err(format!(
                "snapshot checkpoints did not bracket GARM enable and event forward: {observations:?}"
            )
            .into());
        }
        for checkpoint in [SnapshotCheckpoint::PoolEnable, SnapshotCheckpoint::Forward] {
            let observation = self
                .dependencies
                .snapshot_observations()
                .into_iter()
                .find(|observation| observation.checkpoint == checkpoint)
                .ok_or_else(|| format!("snapshot was not read before {checkpoint:?}"))?;
            if let Some(error) = observation.error {
                return Err(format!("snapshot read before {checkpoint:?} failed: {error}").into());
            }
            let snapshot = observation
                .evidence
                .ok_or_else(|| format!("snapshot was not committed before {checkpoint:?}"))?;
            verify_snapshot(&snapshot, "linux", "amd64")?;
            let expected_state = match checkpoint {
                SnapshotCheckpoint::PoolEnable => "disabled",
                SnapshotCheckpoint::Forward => "enabled",
            };
            if snapshot.state != expected_state
                || snapshot.garm_pool_id.as_deref() != Some("pool-system-201")
            {
                return Err(format!(
                    "snapshot was not durably committed before {checkpoint:?}: {snapshot:?}"
                )
                .into());
            }
        }
        Ok(())
    }

    pub(super) async fn complete_run_and_wait_for_pool_drain(
        &self,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let body = serde_json::to_vec(&json!({
            "action": "completed",
            "repository": {"id": 101},
            "workflow_run": {"id": 201, "run_attempt": 1, "conclusion": "success"}
        }))?;
        let mut mac = HookMac::new_from_slice(WEBHOOK_SECRET)?;
        mac.update(&body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/webhooks/github", self.address))
            .header("x-github-event", "workflow_run")
            .header("x-github-delivery", "00000000-0000-4000-8000-000000000202")
            .header("x-hub-signature-256", signature)
            .body(body)
            .send()
            .await?;
        if response.status() != StatusCode::ACCEPTED {
            return Err(
                format!("workflow completion intake returned {}", response.status()).into(),
            );
        }
        for _ in 0..200 {
            let database = self.fixture.directory.join("journal.sqlite3");
            let snapshot =
                tokio::task::spawn_blocking(move || read_snapshot(&database, ROUTING_LABEL))
                    .await??;
            if snapshot.is_some_and(|snapshot| snapshot.state == "deleted") {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err("completed workflow run did not drain its GARM pool".into())
    }

    pub(super) async fn post_signed_event(&self) -> Result<(), Box<dyn std::error::Error>> {
        let body = include_str!("../fixtures/system/workflow-job.json").as_bytes();
        let mut mac = HookMac::new_from_slice(WEBHOOK_SECRET)?;
        mac.update(body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let response = reqwest::Client::new()
            .post(format!("http://{}/v1/webhooks/github", self.address))
            .header("x-github-event", "workflow_job")
            .header("x-github-delivery", "00000000-0000-4000-8000-000000000201")
            .header("x-hub-signature-256", signature)
            .body(body.to_vec())
            .send()
            .await?;
        if response.status() != StatusCode::ACCEPTED {
            return Err(format!("webhook intake returned {}", response.status()).into());
        }
        Ok(())
    }

    pub(super) async fn wait_for_snapshot(
        &self,
    ) -> Result<SnapshotEvidence, Box<dyn std::error::Error>> {
        for _ in 0..100 {
            let database = self.fixture.directory.join("journal.sqlite3");
            let snapshot =
                tokio::task::spawn_blocking(move || read_snapshot(&database, ROUTING_LABEL))
                    .await??;
            if let Some(snapshot) = snapshot {
                return Ok(snapshot);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err("signed webhook did not commit a pool snapshot".into())
    }

    pub(super) fn pool(&self) -> Result<Value, Box<dyn std::error::Error>> {
        self.dependencies
            .state
            .pool
            .lock()
            .map_err(|_| "GARM fixture pool lock poisoned")?
            .clone()
            .ok_or_else(|| "GARM did not create a repository pool".into())
    }

    pub(super) async fn create_runner(&self) -> Result<Output, Box<dyn std::error::Error>> {
        let pool = self.pool()?;
        let mut bootstrap: Value =
            serde_json::from_str(include_str!("../fixtures/system/bootstrap.json"))?;
        bootstrap["pool_id"] = pool["id"].clone();
        bootstrap["extra_specs"] = pool["extra_specs"].clone();
        Ok(provider_process(
            &self.config_path,
            "CreateInstance",
            Some(pool["id"].as_str().ok_or("pool id missing")?),
            Some(&serde_json::to_vec(&bootstrap)?),
        )
        .await?)
    }

    pub(super) async fn restart(&mut self, endpoint: &str) -> io::Result<()> {
        self.daemon.kill()?;
        self.daemon.wait()?;
        self.daemon = start_daemon(
            &self.config_path,
            &self.address,
            endpoint,
            &self.lifecycle_log,
        )
        .await?;
        Ok(())
    }

    pub(super) async fn remove_all(&self) -> Result<Output, Box<dyn std::error::Error>> {
        Ok(provider_process(&self.config_path, "RemoveAllInstances", None, None).await?)
    }
}
