use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use outrider::{
    allocation::catalog_configuration_fingerprint,
    cleanup::{CleanupAttestation, CleanupInspection},
    config::ConfigPatch,
    github::{OwnerRef, RepositoryRef, SnapshotDocument},
    journal::{AllocationAdmission, ChildAttemptRecord, Journal, PoolSnapshot},
    policy::CatalogOffer,
    providers::{ProviderBinding, PurchasingCapability},
};
use serde_json::json;

pub(super) struct TempJournal {
    pub(super) path: PathBuf,
}

impl TempJournal {
    pub(super) fn new(name: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            path: std::env::temp_dir().join(format!(
                "outrider-cleanup-{name}-{}-{suffix}.sqlite",
                std::process::id()
            )),
        }
    }
}

impl Drop for TempJournal {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            self.path.with_extension("lock"),
            self.path.with_extension("sqlite-wal"),
            self.path.with_extension("sqlite-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => eprintln!("remove test journal artifact: {error}"),
            }
        }
    }
}

pub(super) struct CleanupFixture {
    pub(super) temp: TempJournal,
    pub(super) inspection: CleanupInspection,
}

pub(super) struct AttestedCleanupFixture {
    pub(super) fixture: CleanupFixture,
    pub(super) proof: CleanupAttestation,
}

pub(super) fn fixture(name: &str) -> CleanupFixture {
    let temp = TempJournal::new(name);
    let binding = ProviderBinding {
        id: "aws-pin".into(),
        cloud: "aws".into(),
        account: "account-1".into(),
        location: "us-east-1".into(),
        executable_path: "/private/provider-executable".into(),
        executable_sha256: "binary-sha".into(),
        configuration_path: "/private/credential-file".into(),
        configuration_sha256: "configuration-sha".into(),
        source_revision: "provider-revision".into(),
        expected_version: "1.2.3".into(),
        inherited_environment: vec!["AWS_SECRET_ACCESS_KEY".into()],
        supported_os: BTreeSet::from(["linux".into()]),
        supported_architectures: BTreeSet::from(["x86_64".into()]),
        purchasing_capabilities: BTreeSet::from([PurchasingCapability::OnDemand]),
    };
    let catalog = outrider::config::AwsCatalogBinding {
        id: "catalog-1".into(),
        provider_binding_id: binding.id.clone(),
        account_id: binding.account.clone(),
        region: binding.location.clone(),
        pricing_location: "US East".into(),
        credential_reference: "aws-prod".into(),
        locations: BTreeMap::from([(
            "us-east-1".into(),
            outrider::config::AwsNetworkBinding {
                availability_zone: "us-east-1a".into(),
                subnet_id: "subnet-1".into(),
                security_group_ids: Vec::new(),
            },
        )]),
        images: BTreeMap::from([(
            "linux-alias".into(),
            outrider::config::AwsImageBinding {
                image_id: "ami-1".into(),
                os: "linux".into(),
                architecture: "x86_64".into(),
                capabilities: BTreeSet::new(),
                windows_license_included: false,
                cloud_init: true,
                ec2_launch_agent: false,
            },
        )]),
        storage_classes: BTreeMap::new(),
    };
    let central = ConfigPatch {
        provider_bindings: Some(vec![binding.clone()]),
        aws_catalog_bindings: Some(vec![catalog]),
        ..ConfigPatch::default()
    };
    let snapshot = SnapshotDocument {
        base_repository: RepositoryRef {
            id: 7,
            name: "repo".into(),
            full_name: "org/repo".into(),
            owner: OwnerRef {
                login: "org".into(),
            },
        },
        base_repository_id: 7,
        source_repository_id: 7,
        run_id: 11,
        run_attempt: 1,
        job_id: 12,
        commit_sha: "commit-sha".into(),
        central,
        repository: ConfigPatch::default(),
        resolved_profile: serde_json::from_value(json!({
            "os":"linux", "architecture":"x86_64", "image":"ami-1",
            "vcpu_min":1, "vcpu_max":2, "memory_gib_min":1, "memory_gib_max":2,
            "machine_types":{}, "machine_families":{}, "accounts":["account-1"],
            "locations":["us-east-1"], "gpu":null,
            "boot_disk":{"class":"gp3","size_gib":20,"iops":null,"throughput_mib_s":null},
            "local_scratch_gib":0, "capabilities":[], "purchasing":"on_demand",
            "purchasing_models":[], "purchasing_tiers":[], "maximum_total_cost":null,
            "policy":"ordered", "cost_horizon_seconds":3600,"max_price_age_seconds":3600,
            "administrator_limits":{
                "max_vcpu":2,"max_memory_gib":2,"max_boot_disk_gib":20,"max_boot_disk_iops":0,
                "max_boot_disk_throughput_mib_s":0,"max_local_scratch_gib":0,"max_gpu_count":0,
                "max_gpu_memory_gib":0,"max_allocations_controller":10,"max_allocations_account":10,
                "max_allocations_pool":10,"max_total_cost":null,"allowed_clouds":[],
                "allowed_accounts":[],"allowed_locations":[],"allowed_machine_types":{},
                "allowed_machine_families":{},"allowed_gpu_vendors":[],"allowed_gpu_models":[],
                "allowed_os":[],"allowed_architectures":[],"allowed_images":[],
                "allowed_capabilities":[],"allowed_purchasing":[]
            }
        }))
        .unwrap_or_else(|error| panic!("build profile fixture: {error}")),
        image_catalogs: Vec::new(),
    };
    let catalog_fingerprint =
        catalog_configuration_fingerprint(&snapshot.central, "aws", "catalog-1")
            .unwrap_or_else(|error| panic!("fingerprint catalog fixture: {error}"));
    let offer: CatalogOffer = serde_json::from_value(json!({
        "candidate": {
            "identity":"aws/account-1/us-east-1/m6i.large", "os":"linux", "architecture":"x86_64",
            "image":"linux-alias", "vcpu":2, "memory_gib":"8", "machine_type":"m6i.large",
            "machine_family":"m6i", "cloud":"aws", "purchasing":"on_demand", "account":"account-1",
            "location":"us-east-1", "gpu_vendor":null, "gpu_model":null, "gpu_count":0,
            "gpu_memory_gib":null, "boot_disk_class":"gp3", "boot_disk_gib":20,
            "boot_disk_iops":3000, "boot_disk_throughput_mib_s":"125", "local_scratch_gib":"0",
            "capabilities":[], "price":null, "interruption_rate_per_hour":null,
            "allocation_failure_rate":null, "mean_create_seconds":null, "observation_age_seconds":null,
            "observation_ages":null, "observation_window":null, "observation_samples":0,
            "definitive_create_outcomes":0,"measured_latency_samples":0,"interruption_samples":0,
            "native_capacity_hint":null
        },
        "selection": {
            "binding_id":"aws-pin", "source_revision":"provider-revision", "binary_sha256":"binary-sha",
            "configuration_sha256":"configuration-sha", "flavor":"linux", "image":"ami-1",
            "approved_extras":{}
        },
        "launch_facts": {"provider":"aws","facts":{
            "availability_zone":"us-east-1a","subnet_id":"subnet-1","map_public_ip_on_launch":false,
            "root_device_name":"/dev/sda1","root_volume_minimum_gib":20,"maximum_ebs_iops":16000,
            "maximum_ebs_throughput_mib_s":"1000"
        }}
    }))
    .unwrap_or_else(|error| panic!("build offer fixture: {error}"));
    let offer_json = serde_json::to_string(&offer)
        .unwrap_or_else(|error| panic!("serialize offer fixture: {error}"));
    let binding_json = serde_json::to_string(&binding)
        .unwrap_or_else(|error| panic!("serialize binding fixture: {error}"));
    let snapshot_json = serde_json::to_string(&snapshot)
        .unwrap_or_else(|error| panic!("serialize snapshot fixture: {error}"));

    let journal =
        Journal::open(&temp.path).unwrap_or_else(|error| panic!("open fixture journal: {error}"));
    journal
        .save_pool_snapshot(&PoolSnapshot {
            id: "snapshot-1".into(),
            repository_id: 7,
            request_token: "request-1".into(),
            config_json: snapshot_json,
            state: "enabled".into(),
            garm_pool_id: Some("pool-1".into()),
            expires_at: now() + 3600,
        })
        .unwrap_or_else(|error| panic!("save fixture snapshot: {error}"));
    let (_, created) = journal
        .reserve_allocation(
            "allocation-1",
            "controller-1",
            "runner-1",
            "snapshot-1",
            "pool-1",
        )
        .unwrap_or_else(|error| panic!("reserve fixture allocation: {error}"));
    assert!(created);
    assert!(
        journal
            .claim_allocation_selection("allocation-1")
            .unwrap_or_else(|error| panic!("claim fixture selection: {error}"))
    );
    journal
        .set_allocation_selection(
            "allocation-1",
            &offer_json,
            &binding_json,
            "aws:account-1",
            &catalog_fingerprint,
        )
        .unwrap_or_else(|error| panic!("select fixture offer: {error}"));
    journal
        .insert_child_attempt(&ChildAttemptRecord {
            allocation_id: "allocation-1".into(),
            attempt_number: 1,
            state: "reserved".into(),
            offer_json,
            binding_json,
            detail: None,
            created_at: now(),
            dispatched_at: None,
            completed_at: None,
        })
        .unwrap_or_else(|error| panic!("insert fixture child attempt: {error}"));
    assert!(
        journal
            .mark_child_dispatched(
                "allocation-1",
                1,
                &AllocationAdmission {
                    snapshot_id: "snapshot-1".into(),
                    repository_id: 7,
                    run_id: 11,
                    run_attempt: 1,
                    now: now(),
                },
            )
            .unwrap_or_else(|error| panic!("dispatch fixture allocation: {error}"))
    );
    let action = journal
        .begin_allocation_action("controller-1", "runner-1", "DeleteInstance")
        .unwrap_or_else(|error| panic!("start fixture cleanup: {error}"))
        .unwrap_or_else(|| panic!("missing fixture cleanup action"));
    let action_id = action
        .action_id
        .unwrap_or_else(|| panic!("missing action ID"));
    journal
        .finish_allocation_action(action_id, "DeleteInstance", "ambiguous", Some("uncertain"))
        .unwrap_or_else(|error| panic!("finish fixture cleanup: {error}"));
    let inspection = outrider::cleanup::inspect(&journal, "controller-1", "allocation-1")
        .unwrap_or_else(|error| panic!("inspect fixture allocation: {error}"));
    drop(journal);
    CleanupFixture { temp, inspection }
}

pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn cli(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("run Outrider CLI: {error}"))
}

pub(super) fn cli_with_provider_metadata(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(arguments)
        .env("GARM_COMMAND", "GetVersion")
        .env("GARM_INTERFACE_VERSION", "v0.1.0")
        .env("GARM_CONTROLLER_ID", "routing-check")
        .env(
            "GARM_PROVIDER_CONFIG_FILE",
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/config.toml"),
        )
        .output()
        .unwrap_or_else(|error| panic!("run Outrider CLI with provider metadata: {error}"))
}

pub(super) fn attestation(inspection: &CleanupInspection) -> CleanupAttestation {
    CleanupAttestation {
        schema_version: 1,
        allocation_id: inspection.allocation_id.clone(),
        controller_id: inspection.controller_id.clone(),
        pool_id: inspection.pool_id.clone(),
        runner_name: inspection.runner_name.clone(),
        scope_digest: inspection.scope_digest.clone(),
        evidence_reference: "ticket:INC-42#credential-review".into(),
        verified_at_unix_seconds: now(),
        acknowledgements: outrider::cleanup::CleanupAcknowledgements {
            all_cloud_operations_terminal: true,
            no_local_provider_process_can_submit_work: true,
            all_resources_and_dependencies_proved_absent: true,
        },
    }
}

pub(super) fn invoke_inspect(fixture: &CleanupFixture) -> Output {
    invoke_inspect_for_controller(fixture, "controller-1")
}

pub(super) fn invoke_inspect_for_controller(fixture: &CleanupFixture, controller: &str) -> Output {
    cli(&[
        "cleanup",
        "inspect",
        "--database",
        fixture.temp.path.to_str().unwrap_or_default(),
        "--controller",
        controller,
        "--allocation",
        "allocation-1",
    ])
}

pub(super) fn assert_compacted_cleanup_audit(
    fixture: &CleanupFixture,
    proof: &CleanupAttestation,
    summary_json: &str,
) {
    let journal = Journal::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("open aged cleanup fixture: {error}"));
    journal
        .set_pool_snapshot_state("snapshot-1", "deleted", None)
        .unwrap_or_else(|error| panic!("terminalize resolved pool snapshot: {error}"));
    drop(journal);
    let connection = rusqlite::Connection::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("age terminal snapshot fixture: {error}"));
    connection
        .execute(
            "UPDATE pool_snapshots SET terminal_at=1 WHERE id='snapshot-1'",
            [],
        )
        .unwrap_or_else(|error| panic!("age terminal snapshot fixture: {error}"));
    drop(connection);

    let journal = Journal::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("reopen aged cleanup fixture: {error}"));
    assert!(
        journal
            .maintain_retention(0, 32)
            .unwrap_or_else(|error| panic!("compact aged cleanup snapshot: {error}"))
            > 0
    );
    let compacted_snapshot = journal
        .pool_snapshot_by_id("snapshot-1")
        .unwrap_or_else(|error| panic!("read compacted cleanup snapshot: {error}"))
        .unwrap_or_else(|| panic!("compacted cleanup snapshot disappeared"));
    assert!(compacted_snapshot.config_json.contains("compacted"));
    drop(journal);

    let historical_output = invoke_inspect(fixture);
    assert!(
        historical_output.status.success(),
        "{}",
        String::from_utf8_lossy(&historical_output.stderr)
    );
    let historical: CleanupInspection = serde_json::from_slice(&historical_output.stdout)
        .unwrap_or_else(|error| panic!("parse historical cleanup audit: {error}"));
    assert!(historical.historical_audit);
    assert_eq!(historical.allocation_id, proof.allocation_id);
    assert_eq!(historical.controller_id, proof.controller_id);
    assert_eq!(historical.allocation_state, "deleting");
    assert_eq!(historical.scope_digest, proof.scope_digest);
    assert_eq!(
        historical.native_identity_known,
        fixture.inspection.native_identity_known
    );
    assert_eq!(
        historical.provider_configuration_sha256,
        fixture.inspection.provider_configuration_sha256
    );
    assert!(
        !invoke_inspect_for_controller(fixture, "controller-elsewhere")
            .status
            .success()
    );

    let exact_replay = invoke_resolve(fixture, proof);
    assert!(exact_replay.status.success());
    assert_eq!(
        String::from_utf8_lossy(&exact_replay.stdout).trim(),
        "already resolved"
    );
    let mut conflicting_proof = proof.clone();
    conflicting_proof.evidence_reference = "ticket:INC-44#changed-proof".into();
    assert!(!invoke_resolve(fixture, &conflicting_proof).status.success());

    let connection = rusqlite::Connection::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("recheck append-only audit: {error}"));
    let summary_after_compaction: String = connection
        .query_row(
            "SELECT summary_json FROM cleanup_resolution_audit WHERE allocation_id='allocation-1'",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|error| panic!("read audit after compaction: {error}"));
    assert_eq!(summary_after_compaction, summary_json);
}

pub(super) fn inspect_for_attestation(name: &str) -> AttestedCleanupFixture {
    let fixture = fixture(name);
    let output = invoke_inspect(&fixture);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json_text = String::from_utf8_lossy(&output.stdout);
    assert!(!json_text.contains("/private/credential-file"));
    assert!(!json_text.contains("AWS_SECRET_ACCESS_KEY"));
    let inspection: CleanupInspection = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("parse CLI inspection: {error}"));
    assert_eq!(inspection.allocation_state, "deleting");
    assert_eq!(inspection.launch_scope.image_alias, "linux-alias");
    assert_eq!(inspection.launch_scope.native_image_id, "ami-1");
    assert_eq!(
        inspection.launch_scope.availability_zone.as_deref(),
        Some("us-east-1a")
    );
    assert_eq!(
        inspection.launch_scope.subnet_id.as_deref(),
        Some("subnet-1")
    );
    let proof = attestation(&inspection);
    AttestedCleanupFixture { fixture, proof }
}

pub(super) fn invoke_resolve(fixture: &CleanupFixture, proof: &CleanupAttestation) -> Output {
    let proof_path = fixture.temp.path.with_extension("attestation.json");
    fs::write(
        &proof_path,
        serde_json::to_vec(proof).unwrap_or_else(|error| panic!("serialize proof: {error}")),
    )
    .unwrap_or_else(|error| panic!("write proof: {error}"));
    let output = cli(&[
        "cleanup",
        "resolve",
        "--database",
        fixture.temp.path.to_str().unwrap_or_default(),
        "--attestation",
        proof_path.to_str().unwrap_or_default(),
    ]);
    fs::remove_file(proof_path)
        .unwrap_or_else(|error| panic!("remove attestation fixture: {error}"));
    output
}

pub(super) fn assert_cli_refuses_live_journal_owner(
    fixture: &CleanupFixture,
    proof: &CleanupAttestation,
) {
    let journal = Journal::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("lock fixture journal: {error}"));
    assert!(!invoke_inspect(fixture).status.success());
    assert!(!invoke_resolve(fixture, proof).status.success());
    #[cfg(unix)]
    {
        let alias = fixture.temp.path.with_file_name(format!(
            "{}-symlink.sqlite",
            fixture
                .temp
                .path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("journal")
        ));
        std::os::unix::fs::symlink(&fixture.temp.path, &alias)
            .unwrap_or_else(|error| panic!("create database alias: {error}"));
        let aliased = cli(&[
            "cleanup",
            "inspect",
            "--database",
            alias.to_str().unwrap_or_default(),
            "--controller",
            "controller-1",
            "--allocation",
            "allocation-1",
        ]);
        assert!(!aliased.status.success());
        assert!(String::from_utf8_lossy(&aliased.stderr).contains("already running"));
        fs::remove_file(alias).unwrap_or_else(|error| panic!("remove database alias: {error}"));
    }
    assert_eq!(
        journal
            .allocation_by_id("allocation-1")
            .unwrap_or_else(|error| panic!("read locked allocation: {error}"))
            .map(|allocation| allocation.state),
        Some("deleting".into())
    );
}
