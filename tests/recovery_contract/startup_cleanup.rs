use super::*;

pub(super) static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

pub(super) async fn seed_measured_history(harness: &mut allocation_harness::AllocationHarness) {
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap_or_else(|error| panic!("create historical seed allocation: {error}"));
    harness
        .native_inspection_mode
        .store(1, std::sync::atomic::Ordering::Relaxed);
    *harness
        .spot_event_time
        .lock()
        .unwrap_or_else(|_| panic!("set historical fixture event time")) =
        "2020-01-01T00:00:00Z".to_owned();
    assert_eq!(
        harness
            .service
            .observe_interruptions(8)
            .await
            .unwrap_or_else(|_| panic!("record first running observation")),
        0
    );
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(
        harness
            .service
            .observe_interruptions(8)
            .await
            .unwrap_or_else(|_| panic!("record second running observation")),
        0
    );
}

pub(super) fn history_catalog_config(
    harness: &allocation_harness::AllocationHarness,
) -> outrider::config::ConfigPatch {
    let mut central = harness.old_config.clone();
    central.aws_catalog_bindings = Some(
        central
            .aws_catalog_bindings
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|catalog| catalog.id == "aws-east-duplicate")
            .cloned()
            .collect(),
    );
    central
}

pub(super) struct RecoveryFixture {
    pub(super) directory: PathBuf,
    pub(super) database: PathBuf,
}

impl RecoveryFixture {
    pub(super) fn new() -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "outrider-recovery-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap_or_else(|_| panic!("create recovery fixture"));
        let database = directory.join("journal.sqlite3");
        Self {
            directory,
            database,
        }
    }

    pub(super) fn journal(&self) -> Journal {
        Journal::open(&self.database).unwrap_or_else(|_| panic!("open recovery journal"))
    }
}

impl Drop for RecoveryFixture {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.directory) {
            eprintln!("could not remove recovery fixture: {error}");
        }
    }
}

pub(super) fn enabled_snapshot(journal: &Journal) {
    journal
        .save_pool_snapshot(&PoolSnapshot {
            id: "snapshot-recovery".to_owned(),
            repository_id: 7,
            request_token: "route-recovery".to_owned(),
            config_json: "{}".to_owned(),
            state: "enabled".to_owned(),
            garm_pool_id: Some("pool-recovery".to_owned()),
            expires_at: unix_now() + 3600,
        })
        .unwrap_or_else(|_| panic!("save recovery snapshot"));
}

pub(super) fn reserve(journal: &Journal, name: &str) -> String {
    journal
        .reserve_allocation(
            &format!("allocation-{name}"),
            "controller-recovery",
            name,
            "snapshot-recovery",
            "pool-recovery",
        )
        .unwrap_or_else(|_| panic!("reserve recovery allocation"))
        .0
        .allocation_id
}

pub(super) fn dispatch(journal: &Journal, allocation_id: &str) {
    assert!(
        journal
            .claim_allocation_selection(allocation_id)
            .unwrap_or_else(|_| panic!("claim recovery selection"))
    );
    journal
        .insert_child_attempt(&ChildAttemptRecord {
            allocation_id: allocation_id.to_owned(),
            attempt_number: 1,
            state: "reserved".to_owned(),
            offer_json: "{}".to_owned(),
            binding_json: "{}".to_owned(),
            detail: None,
            created_at: unix_now(),
            dispatched_at: None,
            completed_at: None,
        })
        .unwrap_or_else(|_| panic!("reserve recovery child attempt"));
    assert!(
        journal
            .mark_child_dispatched(
                allocation_id,
                1,
                &AllocationAdmission {
                    snapshot_id: "snapshot-recovery".to_owned(),
                    repository_id: 7,
                    run_id: 11,
                    run_attempt: 1,
                    now: unix_now(),
                },
            )
            .unwrap_or_else(|_| panic!("mark recovery dispatch"))
    );
}

pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn persist_dispatched_history_attempt(
    journal: &Journal,
    allocation_id: &str,
    runner_name: &str,
    offer_json: &str,
    binding_json: &str,
    account_id: &str,
    catalog_fingerprint: &str,
) {
    let (allocation, created) = journal
        .reserve_allocation(
            allocation_id,
            "controller-1",
            runner_name,
            "snapshot-history",
            "pool-1",
        )
        .unwrap_or_else(|_| panic!("reserve history test allocation"));
    assert!(created);
    assert!(
        journal
            .claim_allocation_selection(&allocation.allocation_id)
            .unwrap_or_else(|_| panic!("claim history test allocation"))
    );
    journal
        .set_allocation_selection(
            &allocation.allocation_id,
            offer_json,
            binding_json,
            account_id,
            catalog_fingerprint,
        )
        .unwrap_or_else(|_| panic!("persist history test selection"));
    journal
        .insert_child_attempt(&ChildAttemptRecord {
            allocation_id: allocation.allocation_id.clone(),
            attempt_number: 1,
            state: "reserved".to_owned(),
            offer_json: offer_json.to_owned(),
            binding_json: binding_json.to_owned(),
            detail: None,
            created_at: unix_now(),
            dispatched_at: None,
            completed_at: None,
        })
        .unwrap_or_else(|_| panic!("persist history test child attempt"));
    assert!(
        journal
            .mark_child_dispatched(
                &allocation.allocation_id,
                1,
                &AllocationAdmission {
                    snapshot_id: "snapshot-history".to_owned(),
                    repository_id: 7,
                    run_id: 11,
                    run_attempt: 1,
                    now: unix_now(),
                },
            )
            .unwrap_or_else(|_| panic!("mark history test dispatch"))
    );
}

#[tokio::test]
async fn restart_retained_cleanup() {
    allocation_harness::restart_retained_cleanup_case().await;
}

#[tokio::test]
async fn verified_interruption_only() {
    allocation_harness::verified_interruption_only_case().await;
}

#[tokio::test]
async fn observation_provenance() {
    allocation_harness::observation_provenance_case().await;
}

#[tokio::test]
async fn restart_retained_positive_create_absence() {
    allocation_harness::restart_retained_positive_absence_case().await;
}

#[test]
fn startup_closes_pre_spawn_and_fences_dispatched_rows() {
    let fixture = RecoveryFixture::new();
    let journal = fixture.journal();
    enabled_snapshot(&journal);
    let selecting = reserve(&journal, "selecting");
    journal
        .claim_allocation_selection(&selecting)
        .unwrap_or_else(|_| panic!("claim interrupted selection"));
    let unknown = reserve(&journal, "unknown");
    dispatch(&journal, &unknown);
    drop(journal);

    let recovered = fixture.journal();
    recovered
        .recover_startup_allocations()
        .unwrap_or_else(|_| panic!("recover startup allocations"));
    let selecting = recovered
        .allocation("controller-recovery", &selecting)
        .unwrap_or_else(|_| panic!("read pre-spawn row"))
        .unwrap_or_else(|| panic!("pre-spawn row"));
    assert_eq!(selecting.state, "no_candidate");
    assert!(selecting.dispatched_at.is_none());
    let unknown = recovered
        .allocation("controller-recovery", &unknown)
        .unwrap_or_else(|_| panic!("read unresolved row"))
        .unwrap_or_else(|| panic!("unresolved row"));
    assert_eq!(unknown.state, "deleting");
    assert!(unknown.tombstone);
    let action = recovered
        .allocation_action(&unknown.allocation_id)
        .unwrap_or_else(|_| panic!("read recovery action"))
        .unwrap_or_else(|| panic!("recovery action"));
    assert_eq!(action.state, "pending");
    assert!(action.unresolved_create);
}

#[test]
fn startup_preserves_positive_create_and_does_not_delete_healthy_runner() {
    let fixture = RecoveryFixture::new();
    let journal = fixture.journal();
    enabled_snapshot(&journal);
    let positive = reserve(&journal, "positive");
    dispatch(&journal, &positive);
    journal
        .preserve_ambiguous_child_result(
            &positive,
            r#"{"provider_id":"child-positive"}"#,
            "native lookup unavailable",
        )
        .unwrap_or_else(|_| panic!("preserve positive child return"));

    let healthy = reserve(&journal, "healthy");
    dispatch(&journal, &healthy);
    let identity = PlacementIdentity {
        controller_id: "controller-recovery".to_owned(),
        cloud: "aws".to_owned(),
        account_id: "account-recovery".to_owned(),
        location: "us-east-1".to_owned(),
        resource_id: "i-healthy".to_owned(),
        host_id: None,
        dependency_ids: Default::default(),
    };
    journal
        .complete_allocation(
            &healthy,
            r#"{"provider_id":"child-healthy"}"#,
            &serde_json::to_string(&identity).unwrap_or_else(|_| panic!("encode identity")),
        )
        .unwrap_or_else(|_| panic!("complete healthy allocation"));
    drop(journal);

    let recovered = fixture.journal();
    recovered
        .recover_startup_allocations()
        .unwrap_or_else(|_| panic!("recover startup allocations"));
    let positive = recovered
        .allocation("controller-recovery", &positive)
        .unwrap_or_else(|_| panic!("read positive row"))
        .unwrap_or_else(|| panic!("positive row"));
    assert_eq!(positive.state, "deleting");
    assert!(positive.tombstone);
    assert!(positive.returned_at.is_some());
    assert_eq!(
        positive.provider_instance_json.as_deref(),
        Some(r#"{"provider_id":"child-positive"}"#)
    );
    let cleanup = recovered
        .allocation_action(&positive.allocation_id)
        .unwrap_or_else(|_| panic!("read positive cleanup action"))
        .unwrap_or_else(|| panic!("positive cleanup action"));
    assert_eq!(cleanup.state, "pending");
    assert!(!cleanup.unresolved_create);
    let healthy = recovered
        .allocation("controller-recovery", &healthy)
        .unwrap_or_else(|_| panic!("read healthy row"))
        .unwrap_or_else(|| panic!("healthy row"));
    assert_eq!(healthy.state, "created");
    assert!(!healthy.tombstone);
}

pub(super) async fn continuity_history_harness() -> allocation_harness::AllocationHarness {
    use outrider::{
        journal::PoolSnapshot,
        policy::{MissingDataBehavior, Policy, PolicyDefinition},
    };

    let mut harness =
        allocation_harness::AllocationHarness::new(allocation_harness::AllocationHarnessOptions {
            spot: true,
            child_mode: "multiple_slow_delete".to_owned(),
            provider_timeout_seconds: 1,
            ..allocation_harness::AllocationHarnessOptions::default()
        })
        .await;
    seed_measured_history(&mut harness).await;

    let mut central = history_catalog_config(&harness);
    central.default_policy = Some("continuity".to_owned());
    central
        .policies
        .as_mut()
        .unwrap_or_else(|| panic!("policy definitions"))
        .insert(
            "continuity".to_owned(),
            PolicyDefinition {
                strategy: Policy::Continuity {
                    max_observation_age_seconds: 600,
                    min_samples: 1,
                },
                missing_data: MissingDataBehavior::Reject,
            },
        );
    let mut document = allocation_harness::snapshot_document(central, "history-token");
    document.resolved_profile.policy = "continuity".to_owned();
    document.resolved_profile.purchasing_models =
        std::collections::BTreeSet::from(["spot".to_owned()]);
    harness
        .journal
        .save_pool_snapshot(&PoolSnapshot {
            id: "snapshot-history".to_owned(),
            repository_id: 7,
            request_token: "history-token".to_owned(),
            config_json: serde_json::to_string(&document)
                .unwrap_or_else(|_| panic!("serialize historical policy snapshot")),
            state: "enabled".to_owned(),
            garm_pool_id: Some("pool-1".to_owned()),
            expires_at: unix_now() + 3600,
        })
        .unwrap_or_else(|_| panic!("persist historical policy snapshot"));
    harness
}

#[tokio::test]
async fn historical_policy_inputs_after_restart() {
    let mut harness = continuity_history_harness().await;
    let source_allocation = harness.allocation();
    harness
        .journal
        .set_allocation_state(&source_allocation.allocation_id, "deleted", None)
        .unwrap_or_else(|_| panic!("retain verified success after cleanup"));
    let successes = harness
        .journal
        .unknown_create_attempts(
            u128::from(unix_now().saturating_sub(60)) * 1000,
            u128::from(unix_now()) * 1000,
        )
        .unwrap_or_else(|_| panic!("read unknown creates after cleanup"));
    assert!(successes.is_empty());
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
    let reopened = std::sync::Arc::new(harness._fixture.journal());
    let service = Box::pin(allocation_harness::rebuild_allocation_service(
        &harness._fixture,
        reopened.clone(),
        &harness.aws_endpoint,
        &harness.old_config,
        &harness.current_config,
        "multiple_slow_delete",
    ))
    .await;
    harness.journal.0 = Some(reopened);
    harness.service.0 = Some(service);
    let mut request = harness.request.clone();
    let bootstrap = request
        .bootstrap
        .as_mut()
        .unwrap_or_else(|| panic!("history bootstrap"));
    bootstrap.name = "runner-second".to_owned();
    bootstrap.labels = vec![
        "history-token".to_owned(),
        "runner-pool-id=pool-1".to_owned(),
        "runner-controller-id=controller-1".to_owned(),
    ];
    bootstrap.extra_specs = Some(serde_json::json!({"snapshot_id":"snapshot-history"}));
    request.pool_extra_specs = Some(serde_json::json!({"snapshot_id":"snapshot-history"}));
    harness
        .service
        .handle(request)
        .await
        .unwrap_or_else(|error| panic!("rank verified historical policy inputs: {error}"));
}
