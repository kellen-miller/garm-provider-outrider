use super::*;

pub(super) fn queued_placeholder_job() -> Value {
    json!({
        "id":301,"run_id":201,"run_attempt":1,"head_sha":SHA,
        "status":"queued","runner_name":"","runner_id":0,
        "started_at":null,"completed_at":null
    })
}

pub(super) fn in_progress_assigned_job() -> Value {
    json!({
        "id":301,"run_id":201,"run_attempt":1,"head_sha":SHA,
        "status":"in_progress","runner_name":"runner-301","runner_id":9001,
        "started_at":"2026-10-21T00:00:00Z","completed_at":null
    })
}

pub(super) fn completed_failure_body() -> Vec<u8> {
    json!({
        "action":"completed",
        "repository":{"id":101,"name":"base-repo","full_name":"base-owner/base-repo","owner":{"login":"base-owner"}},
        "workflow_job":{"id":301,"run_id":201,"run_attempt":1,"head_sha":SHA,"status":"completed","conclusion":"failure","runner_name":"runner-301","runner_id":9001,"started_at":"2026-10-21T00:00:00Z","completed_at":"2026-10-21T00:01:00Z","labels":[TOKEN]}
    }).to_string().into_bytes()
}

pub(super) fn in_progress_body() -> Vec<u8> {
    json!({
        "action":"in_progress",
        "repository":{"id":REPOSITORY_ID,"name":"base-repo","full_name":"base-owner/base-repo","owner":{"login":"base-owner"}},
        "workflow_job":{"id":301,"run_id":201,"run_attempt":1,"head_sha":SHA,"status":"in_progress","runner_name":"runner-301","runner_id":9001,"started_at":"2026-10-21T00:00:00Z","labels":[TOKEN]}
    })
    .to_string()
    .into_bytes()
}

pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub(super) fn enabled_rerun_config(settings: IngressSettings) -> ConfigPatch {
    let mut config = config_with_ingress(settings);
    config.reruns = Some(RerunSettings {
        enabled: true,
        max_per_run: 2,
        actions_write_permission: true,
    });
    config
}

pub(super) fn seed_interrupted_assignment(journal: &Journal, settings: &IngressSettings) {
    let mut snapshot = draining_pool_snapshot(settings);
    snapshot.state = "enabled".into();
    journal
        .save_pool_snapshot(&snapshot)
        .unwrap_or_else(|_| panic!("save rerun snapshot"));
    let (allocation, _) = journal
        .reserve_allocation(
            "allocation-rerun",
            CONTROLLER_ID,
            "runner-301",
            &snapshot.id,
            "pool-201",
        )
        .unwrap_or_else(|_| panic!("reserve assigned runner"));
    assert!(
        journal
            .claim_allocation_selection(&allocation.allocation_id)
            .unwrap_or_else(|_| panic!("claim runner allocation"))
    );
    journal
        .set_allocation_selection(
            &allocation.allocation_id,
            "{}",
            "{}",
            "account-1",
            "catalog-1",
        )
        .unwrap_or_else(|_| panic!("select assigned runner"));
    journal
        .insert_child_attempt(&ChildAttemptRecord {
            allocation_id: allocation.allocation_id.clone(),
            attempt_number: 1,
            state: "reserved".into(),
            offer_json: "{}".into(),
            binding_json: "{}".into(),
            detail: None,
            created_at: unix_now(),
            dispatched_at: None,
            completed_at: None,
        })
        .unwrap_or_else(|_| panic!("reserve assigned child attempt"));
    assert!(
        journal
            .mark_child_dispatched(
                &allocation.allocation_id,
                1,
                &AllocationAdmission {
                    snapshot_id: snapshot.id,
                    repository_id: REPOSITORY_ID,
                    run_id: 201,
                    run_attempt: 1,
                    now: unix_now(),
                },
            )
            .unwrap_or_else(|_| panic!("dispatch assigned child"))
    );
    let identity = PlacementIdentity {
        controller_id: CONTROLLER_ID.into(),
        cloud: "aws".into(),
        account_id: "account-1".into(),
        location: "us-east-1".into(),
        resource_id: "i-rerun".into(),
        host_id: None,
        dependency_ids: BTreeMap::new(),
    };
    let identity_json =
        serde_json::to_string(&identity).unwrap_or_else(|_| panic!("encode runner identity"));
    assert!(
        journal
            .complete_allocation(&allocation.allocation_id, "{}", &identity_json)
            .unwrap_or_else(|_| panic!("complete runner allocation"))
    );
    journal
        .record_interruption(
            &InterruptionRecord {
                allocation_id: allocation.allocation_id,
                controller_id: CONTROLLER_ID.into(),
                pool_id: "pool-201".into(),
                catalog_fingerprint: "catalog-1".into(),
                configuration_fingerprint: "config-1".into(),
                provider_pin_json: "{}".into(),
                identity_json,
                source_api: "fixture".into(),
                api_version: "v1".into(),
                source_id: "source-1".into(),
                event_id: "event-1".into(),
                occurred_at: "2026-10-21T00:00:30Z".into(),
                observed_at: "2026-10-21T00:00:31Z".into(),
                reason_fields_json: "{}".into(),
            },
            &identity,
        )
        .unwrap_or_else(|_| panic!("persist provider interruption"));
}

pub(super) fn fork_run() -> Value {
    json!({
        "id":201,"run_attempt":1,"event":"pull_request","head_sha":SHA,"status":"in_progress","conclusion":null,
        "repository":{"id":101,"name":"base-repo","full_name":"base-owner/base-repo","owner":{"login":"base-owner"}},
        "head_repository":{"id":102,"name":"base-repo","full_name":"fork-owner/base-repo","owner":{"login":"fork-owner"}}
    })
}

pub(super) fn push_run() -> Value {
    json!({
        "id":201,"run_attempt":1,"event":"push","head_sha":SHA,"status":"in_progress","conclusion":null,
        "repository":{"id":101,"name":"base-repo","full_name":"base-owner/base-repo","owner":{"login":"base-owner"}},
        "head_repository":{"id":101,"name":"base-repo","full_name":"base-owner/base-repo","owner":{"login":"base-owner"}}
    })
}

pub(super) fn job_response() -> Value {
    json!({"id":301,"run_id":201,"run_attempt":1,"head_sha":SHA,"status":"queued","runner_name":null,"runner_id":null,"started_at":null,"completed_at":null})
}

pub(super) fn profile() -> RunnerProfile {
    RunnerProfile {
        os: "linux".into(),
        architecture: "amd64".into(),
        image: "ubuntu-22".into(),
        vcpu_min: 2,
        vcpu_max: 2,
        memory_gib_min: 4,
        memory_gib_max: 4,
        machine_types: BTreeMap::new(),
        machine_families: BTreeMap::new(),
        accounts: vec![],
        locations: vec![],
        gpu: None,
        boot_disk: outrider::config::DiskRequirement {
            class: "standard".into(),
            size_gib: 30,
            iops: None,
            throughput_mib_s: None,
        },
        local_scratch_gib: 0,
        capabilities: BTreeSet::new(),
        purchasing: "on_demand".into(),
        purchasing_models: BTreeSet::from(["on_demand".into()]),
        purchasing_tiers: vec![vec!["on_demand".into()]],
        maximum_total_cost: None,
        policy: "economy".into(),
        cost_horizon_seconds: 3600,
        max_price_age_seconds: 300,
        administrator_limits: AdministratorLimits::default(),
    }
}

pub(super) fn ingress_settings(
    directory: &TestDirectory,
    github_origin: &str,
    garm_origin: &str,
) -> io::Result<IngressSettings> {
    let github_token = directory.write("github-token", b"github-token")?;
    let garm_token = directory.write("garm-token", b"garm-token")?;
    let webhook_secret = directory.write("webhook-secret", SECRET)?;
    let repositories = [(
        REPOSITORY_ID.to_string(),
        IngressRepository {
            garm_repository_id: GARM_REPOSITORY_ID.into(),
            webhook_secret_file: webhook_secret.display().to_string(),
            allow_forks: false,
            template_id: Some(9),
        },
    )]
    .into_iter()
    .collect();
    Ok(IngressSettings {
        github_api_origin: github_origin.to_owned(),
        github_token_file: github_token.display().to_string(),
        garm_api_origin: garm_origin.to_owned(),
        garm_token_file: garm_token.display().to_string(),
        controller_id: CONTROLLER_ID.into(),
        provider_name: "outrider".into(),
        pool_flavor: "garm-flavor".into(),
        pool_image: "garm-image".into(),
        runner_prefix: "outrider".into(),
        max_dynamic_pools: 4,
        max_runners_per_pool: 4,
        allocation_timeout_seconds: 300,
        bootstrap_timeout_minutes: 20,
        timeout_margin_seconds: 30,
        pool_ttl_seconds: 3600,
        webhook_max_bytes: 1024 * 1024,
        repositories,
    })
}

pub(super) fn config_with_ingress(settings: IngressSettings) -> ConfigPatch {
    ConfigPatch {
        ingress: Some(settings),
        ..ConfigPatch::default()
    }
}

pub(super) fn full_ingress_config(settings: IngressSettings) -> ConfigPatch {
    let mut config = ConfigPatch::from_toml(
        "cost_horizon_seconds = 3600\ndefault_policy = \"economy\"\ndefault_purchasing = \"on_demand\"\nmax_price_age_seconds = 300\nversion = 1\n\n[defaults]\naccounts = [\"build-account\"]\narchitecture = \"amd64\"\ncapabilities = []\nimage = \"ubuntu-22\"\nlocal_scratch_gib = 0\nlocations = [\"us-east-1\"]\nmemory_gib_max = 4\nmemory_gib_min = 4\nos = \"linux\"\nvcpu_max = 2\nvcpu_min = 2\n\n[defaults.boot_disk]\nclass = \"standard\"\nsize_gib = 30\n\n[defaults.machine_types]\naws = [\"m7i.large\"]\n\n[limits]\nallowed_accounts = [\"build-account\"]\nallowed_architectures = [\"amd64\"]\nallowed_capabilities = []\nallowed_clouds = [\"aws\"]\nallowed_gpu_models = []\nallowed_gpu_vendors = []\nallowed_images = [\"ubuntu-22\"]\nallowed_locations = [\"us-east-1\"]\nallowed_os = [\"linux\"]\nallowed_purchasing = [\"on_demand\"]\nmax_allocations_account = 20\nmax_allocations_controller = 100\nmax_allocations_pool = 10\nmax_boot_disk_gib = 256\nmax_boot_disk_iops = 0\nmax_boot_disk_throughput_mib_s = 0\nmax_gpu_count = 0\nmax_gpu_memory_gib = 0\nmax_local_scratch_gib = 0\nmax_memory_gib = 32\nmax_vcpu = 8\n\n[limits.allowed_machine_families]\n\n[limits.allowed_machine_types]\naws = [\"m7i.large\"]\n\n[policies.economy.missing_data]\naction = \"reject\"\n\n[policies.economy.strategy]\nmax_price_age_seconds = 300\nstrategy = \"economy\"\n\n[profiles.build]\n",
    )
    .unwrap_or_else(|_| panic!("valid administrator configuration"));
    config.ingress = Some(settings);
    config.aws_catalog_bindings = Some(vec![AwsCatalogBinding {
        id: "aws-catalog".into(),
        provider_binding_id: "aws-provider".into(),
        account_id: "build-account".into(),
        region: "us-east-1".into(),
        pricing_location: "US East".into(),
        credential_reference: "provider-binding:aws-provider:configuration".into(),
        locations: BTreeMap::new(),
        images: BTreeMap::from([(
            "ubuntu-22".into(),
            AwsImageBinding {
                image_id: "ami-frozen".into(),
                os: "linux".into(),
                architecture: "amd64".into(),
                capabilities: BTreeSet::new(),
                windows_license_included: false,
                cloud_init: true,
                ec2_launch_agent: false,
            },
        )]),
        storage_classes: BTreeMap::new(),
    }]);
    config
}

pub(super) fn open_service(
    directory: &TestDirectory,
    github_origin: &str,
    garm_origin: &str,
) -> io::Result<(IngressService, Arc<Journal>)> {
    let journal = directory.journal().map_err(io::Error::other)?;
    let settings = ingress_settings(directory, github_origin, garm_origin)?;
    let service = IngressService::new(config_with_ingress(settings), journal.clone())
        .map_err(io::Error::other)?;
    Ok((service, journal))
}

pub(super) fn sign(body: &[u8]) -> String {
    let Ok(mut mac) = TestMac::new_from_slice(SECRET) else {
        return String::new();
    };
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

pub(super) fn webhook(
    event: &str,
    delivery_id: &str,
    body: &[u8],
) -> (String, BTreeMap<String, String>) {
    let signature = sign(body);
    let headers = [
        ("X-GitHub-Event".into(), event.into()),
        ("X-GitHub-Delivery".into(), delivery_id.into()),
        (
            "X-GitHub-Hook-Installation-Target-Type".into(),
            "integration".into(),
        ),
        ("X-Hub-Signature-256".into(), signature.clone()),
    ]
    .into_iter()
    .collect();
    (signature, headers)
}

pub(super) fn queued_body(labels: Vec<&str>) -> Vec<u8> {
    json!({
        "action":"queued", "repository":{"id":101,"name":"base-repo","full_name":"base-owner/base-repo","owner":{"login":"base-owner"}},
        "workflow_job":{"id":301,"run_id":201,"run_attempt":1,"head_sha":SHA,"status":"queued","labels":labels}
    }).to_string().into_bytes()
}

#[tokio::test]
async fn signed_durable_ack() {
    let directory = TestDirectory::new().expect("temp directory");
    let (service, journal) = open_service(&directory, "http://127.0.0.1:1/", "http://127.0.0.1:1/")
        .expect("service config");
    let body = br#"{"repository":{"id":101,"name":"base-repo","full_name":"base-owner/base-repo","owner":{"login":"base-owner"}},"action":"created"}"#;
    let (signature, headers) = webhook("repository", "delivery-1", body);
    assert!(
        service
            .intake(
                "workflow_job",
                "delivery-1",
                &signature,
                headers.clone(),
                body
            )
            .is_err()
    );
    assert!(
        service
            .intake("repository", "delivery-1", &signature, headers, body)
            .expect("durable intake")
    );
    assert_eq!(journal.pending_deliveries(10).expect("inbox").len(), 1);
    assert_eq!(journal.pending_deliveries(10).expect("inbox")[0].body, body);
}

#[tokio::test]
async fn exact_repository_pool_before_forward() {
    let directory = TestDirectory::new().expect("temp directory");
    let server = HttpFixture::start(FixtureMode::GarmPool)
        .await
        .expect("HTTP fixture");
    let mut settings =
        ingress_settings(&directory, "http://127.0.0.1:1/", &server.origin).expect("settings");
    settings.garm_api_origin = server.origin.trim_end_matches('/').to_owned();
    let journal = directory.journal().expect("journal");
    let document = outrider::github::SnapshotDocument {
        base_repository: RepositoryRef {
            id: REPOSITORY_ID,
            name: "base-repo".into(),
            full_name: "base-owner/base-repo".into(),
            owner: OwnerRef {
                login: "base-owner".into(),
            },
        },
        base_repository_id: REPOSITORY_ID,
        source_repository_id: REPOSITORY_ID,
        run_id: 201,
        run_attempt: 1,
        job_id: 301,
        commit_sha: SHA.into(),
        central: config_with_ingress(settings.clone()),
        repository: ConfigPatch::default(),
        resolved_profile: profile(),
        image_catalogs: vec![],
    };
    let snapshot = PoolSnapshot {
        id: snapshot_id(),
        repository_id: REPOSITORY_ID,
        request_token: TOKEN.into(),
        config_json: serde_json::to_string(&document).expect("snapshot encoding"),
        state: "prepared".into(),
        garm_pool_id: None,
        expires_at: 2_000_000_000,
    };
    let client = GarmClient::from_settings(&settings, std::time::Duration::from_secs(3))
        .expect("GARM client");
    client
        .ensure_repository_pool(
            &journal,
            RepositoryPoolRequest {
                snapshot: &snapshot,
                admission: None,
            },
        )
        .await
        .expect("pool reconciliation");
    let body = queued_body(vec![TOKEN]);
    let signature = sign(&body);
    let headers = vec![
        ("x-github-event".into(), "workflow_job".into()),
        ("x-github-delivery".into(), "delivery-2".into()),
        (
            "x-github-hook-installation-target-type".into(),
            "integration".into(),
        ),
        ("x-hub-signature-256".into(), signature.clone()),
    ];
    client
        .forward_webhook(CONTROLLER_ID, &headers, &body)
        .await
        .expect("forward signed bytes");
    let requests = server.requests();
    let paths = requests
        .iter()
        .map(|request| request.path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        paths[0],
        "/api/v1/repositories/00000000-0000-0000-0000-000000000101"
    );
    assert!(
        paths
            .iter()
            .position(|path| *path
                == "/api/v1/repositories/00000000-0000-0000-0000-000000000101/pools/pool-201")
            .is_some()
    );
    let saved = journal
        .pool_snapshot(REPOSITORY_ID, TOKEN)
        .expect("snapshot query")
        .expect("durable snapshot");
    assert_eq!(saved.state, "enabled");
    let create = requests
        .iter()
        .find(|request| request.method == "POST" && request.path.ends_with("/pools"))
        .expect("create request");
    let create_body: Value = serde_json::from_slice(&create.body).expect("create DTO");
    assert_eq!(create_body["enabled"], false);
    assert_eq!(create_body["min_idle_runners"], 0);
    assert_eq!(create_body["tags"], json!([TOKEN]));
    assert_eq!(create_body["runner_bootstrap_timeout"], 26);
    let forward = requests
        .iter()
        .find(|request| request.path == "/webhooks/00000000-0000-0000-0000-000000000901")
        .expect("webhook forwarding");
    assert_eq!(forward.body, body);
    assert_eq!(forward.signature.as_deref(), Some(signature.as_str()));
    assert_eq!(forward.authorization, None);
    assert_eq!(forward.hook_target.as_deref(), Some("repository"));
}

#[tokio::test]
async fn full_signed_out_rider_pipeline() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(push_run(), job_response(), true)
        .await
        .expect("GitHub API fixture");
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM API fixture");
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let service = IngressService::new(full_ingress_config(settings), journal.clone())
        .expect("production ingress service");
    let body = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "delivery-positive", &body);
    assert!(
        service
            .intake(
                "workflow_job",
                "delivery-positive",
                &signature,
                headers,
                &body
            )
            .expect("authenticated durable intake")
    );
    assert_eq!(
        service.process_pending(1).await.expect("complete pipeline"),
        2
    );

    let snapshot = journal
        .pool_snapshot(REPOSITORY_ID, TOKEN)
        .expect("snapshot query")
        .expect("persisted admission snapshot");
    assert_eq!(snapshot.state, "enabled");
    assert!(garm.state.snapshot_visible_at_create.load(Ordering::SeqCst));
    let document: outrider::github::SnapshotDocument =
        serde_json::from_str(&snapshot.config_json).expect("immutable snapshot document");
    assert_eq!(
        document.image_catalogs[0]["image"]["image_id"],
        "ami-frozen"
    );

    let requests = garm.requests();
    let create_index = requests
        .iter()
        .position(|request| request.method == "POST" && request.path.ends_with("/pools"))
        .expect("pool create");
    let enable_index = requests
        .iter()
        .position(|request| request.method == "PUT" && request.path.ends_with("/pool-201"))
        .expect("pool enable");
    let forward_index = requests
        .iter()
        .position(|request| request.path == "/webhooks/00000000-0000-0000-0000-000000000901")
        .expect("original event forwarding");
    assert!(create_index < enable_index && enable_index < forward_index);
    let forward = &requests[forward_index];
    assert_eq!(forward.body, body);
    assert_eq!(forward.signature.as_deref(), Some(signature.as_str()));
    assert_eq!(forward.authorization, None);
    assert_eq!(forward.hook_target.as_deref(), Some("repository"));
    assert!(journal.pending_outbox(10).expect("outbox").is_empty());
}
