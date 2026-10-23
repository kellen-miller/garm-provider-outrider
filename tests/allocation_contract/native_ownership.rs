use super::*;

#[tokio::test]
async fn list_outage_not_empty() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        current_max_vcpu: 32,
        current_account_allowed: true,
        max_allocations_account: 100,
        snapshot_state: "enabled".to_owned(),
        expires_in_seconds: 3600,
        allocation_timeout_seconds: 600,
        provider_timeout_seconds: 5,
        spot: false,
        child_mode: "normal".to_owned(),
    })
    .await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap_or_else(|_| panic!("create"));
    fs::remove_file(harness._fixture.directory.join("provider-process"))
        .unwrap_or_else(|_| panic!("remove pinned provider executable"));
    let mut list = harness.request.clone();
    list.command = "ListInstances".to_owned();
    list.bootstrap = None;
    list.instance_id = None;
    list.pool_extra_specs = None;
    assert!(
        harness.service.handle(list).await.is_err(),
        "provider outage must not become an empty pool"
    );
}

#[tokio::test]
async fn native_id_binding_collision() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        current_max_vcpu: 32,
        current_account_allowed: true,
        max_allocations_account: 100,
        snapshot_state: "enabled".to_owned(),
        expires_in_seconds: 3600,
        allocation_timeout_seconds: 600,
        provider_timeout_seconds: 5,
        spot: false,
        child_mode: "normal".to_owned(),
    })
    .await;
    assert!(
        harness
            .service
            .handle(harness.request.clone())
            .await
            .is_ok()
    );
    let mut second = harness.request.clone();
    second
        .bootstrap
        .as_mut()
        .unwrap_or_else(|| panic!("bootstrap"))
        .name = "runner-collision".to_owned();
    assert!(matches!(
        harness.service.handle(second).await,
        Err(AllocationError::Ambiguous)
    ));
    let collision = harness
        .journal
        .allocation("controller-1", "runner-collision")
        .unwrap_or_else(|_| panic!("collision lookup"))
        .unwrap_or_else(|| panic!("collision row"));
    assert_eq!(collision.state, "deleting");
    assert!(collision.tombstone);
    assert!(
        harness
            .collision_native_observations
            .load(Ordering::Relaxed)
            > 0,
        "the second binding must observe the exact controller, pool, name, and native ID"
    );
    let second_attempts = harness
        .journal
        .child_attempts(&collision.allocation_id)
        .unwrap_or_else(|_| panic!("second allocation attempts"));
    assert_eq!(second_attempts.len(), 1);
    assert_eq!(second_attempts[0].state, "ambiguous");
    assert_eq!(
        second_attempts[0].detail.as_deref(),
        Some("native identity is already claimed by another allocation")
    );
    let mut delete = harness.request.clone();
    delete.command = "DeleteInstance".to_owned();
    delete.instance_id = Some("runner-collision".to_owned());
    delete.bootstrap = None;
    delete.pool_extra_specs = None;
    assert!(matches!(
        harness.service.handle(delete).await,
        Err(AllocationError::Ambiguous)
    ));
    assert!(
        !harness
            .lifecycle_commands()
            .contains(&"DeleteInstance".to_owned()),
        "a conflicting allocation must not delete the claimed VM"
    );
    assert_eq!(
        harness.allocation().state,
        "created",
        "the first allocation must retain ownership"
    );
}

#[tokio::test]
async fn name_and_opaque_id_same_allocation() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        current_max_vcpu: 32,
        current_account_allowed: true,
        max_allocations_account: 100,
        snapshot_state: "enabled".to_owned(),
        expires_in_seconds: 3600,
        allocation_timeout_seconds: 600,
        provider_timeout_seconds: 5,
        spot: false,
        child_mode: "normal".to_owned(),
    })
    .await;
    let created = harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap_or_else(|_| panic!("create"))
        .unwrap_or_else(|| panic!("instance"));
    let opaque_id = created["provider_id"]
        .as_str()
        .unwrap_or_else(|| panic!("opaque allocation ID"))
        .to_owned();
    let by_name = harness
        .journal
        .allocation("controller-1", "runner-old-image")
        .unwrap_or_else(|_| panic!("name lookup"))
        .unwrap_or_else(|| panic!("allocation"));
    let by_id = harness
        .journal
        .allocation("controller-1", &opaque_id)
        .unwrap_or_else(|_| panic!("opaque lookup"))
        .unwrap_or_else(|| panic!("allocation"));
    assert_eq!(by_name.allocation_id, by_id.allocation_id);
    let mut get = harness.request.clone();
    get.command = "GetInstance".to_owned();
    get.instance_id = Some(opaque_id.clone());
    get.pool_id = None;
    get.bootstrap = None;
    get.pool_extra_specs = None;
    let result = harness
        .service
        .handle(get)
        .await
        .unwrap_or_else(|_| panic!("get by opaque ID"))
        .unwrap_or_else(|| panic!("instance"));
    assert_eq!(result["provider_id"], opaque_id);
}

#[tokio::test]
async fn wrapped_bootstrap_limit_pre_spawn() {
    let mut harness = AllocationHarness::new(AllocationHarnessOptions {
        current_max_vcpu: 32,
        current_account_allowed: true,
        max_allocations_account: 100,
        snapshot_state: "enabled".to_owned(),
        expires_in_seconds: 3600,
        allocation_timeout_seconds: 600,
        provider_timeout_seconds: 5,
        spot: false,
        child_mode: "record".to_owned(),
    })
    .await;
    harness
        .request
        .bootstrap
        .as_mut()
        .unwrap_or_else(|| panic!("bootstrap"))
        .ssh_keys = vec!["x".repeat(2 * 1024 * 1024)];
    assert!(
        harness
            .service
            .handle(harness.request.clone())
            .await
            .is_err()
    );
    assert!(harness.lifecycle_commands().is_empty());
}

#[tokio::test]
async fn remove_all_owned_journal_only() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        current_max_vcpu: 32,
        current_account_allowed: true,
        max_allocations_account: 100,
        snapshot_state: "enabled".to_owned(),
        expires_in_seconds: 3600,
        allocation_timeout_seconds: 600,
        provider_timeout_seconds: 5,
        spot: false,
        child_mode: "normal".to_owned(),
    })
    .await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap_or_else(|_| panic!("create"));
    let removed = harness
        .service
        .handle(ProviderRequest {
            command: "RemoveAllInstances".to_owned(),
            interface_version: outrider::providers::GARM_INTERFACE_VERSION.to_owned(),
            controller_id: "controller-1".to_owned(),
            pool_id: None,
            instance_id: None,
            bootstrap: None,
            pool_extra_specs: None,
        })
        .await;
    assert!(removed.is_ok());
    assert_eq!(harness.allocation().state, "deleted");
    let commands = harness.lifecycle_commands();
    assert!(commands.contains(&"DeleteInstance".to_owned()));
    assert!(!commands.contains(&"RemoveAllInstances".to_owned()));
}

#[tokio::test]
async fn no_bare_name_native_mutation() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        current_max_vcpu: 32,
        current_account_allowed: true,
        max_allocations_account: 100,
        snapshot_state: "enabled".to_owned(),
        expires_in_seconds: 3600,
        allocation_timeout_seconds: 600,
        provider_timeout_seconds: 5,
        spot: false,
        child_mode: "normal".to_owned(),
    })
    .await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap_or_else(|_| panic!("create"));
    let mut delete = harness.request.clone();
    delete.command = "DeleteInstance".to_owned();
    delete.instance_id = Some("runner-old-image".to_owned());
    delete.bootstrap = None;
    delete.pool_extra_specs = None;
    assert!(harness.service.handle(delete).await.is_ok());
    let records =
        fs::read_to_string(&harness.lifecycle_log).unwrap_or_else(|_| panic!("lifecycle records"));
    let delete_record = records
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["command"] == "DeleteInstance")
        .unwrap_or_else(|| panic!("delete child request"));
    assert_eq!(delete_record["instance_id"], "i-synthetic-001");
}

#[tokio::test]
async fn ambiguous_counts_against_caps() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        current_max_vcpu: 32,
        current_account_allowed: true,
        max_allocations_account: 1,
        snapshot_state: "enabled".to_owned(),
        expires_in_seconds: 3600,
        allocation_timeout_seconds: 600,
        provider_timeout_seconds: 5,
        spot: false,
        child_mode: "exit1".to_owned(),
    })
    .await;
    assert!(matches!(
        harness.service.handle(harness.request.clone()).await,
        Err(AllocationError::Ambiguous)
    ));
    let mut second = harness.request.clone();
    second
        .bootstrap
        .as_mut()
        .unwrap_or_else(|| panic!("bootstrap"))
        .name = "runner-second".to_owned();
    assert!(matches!(
        harness.service.handle(second).await,
        Err(AllocationError::Limit)
    ));
    assert_eq!(
        harness
            .lifecycle_commands()
            .iter()
            .filter(|command| command.as_str() == "CreateInstance")
            .count(),
        1
    );
}

pub(super) fn record_ambiguous_account_allocation(
    journal: &Journal,
    allocation_id: &str,
    runner_name: &str,
    account_scope: &str,
) {
    journal
        .save_pool_snapshot(&PoolSnapshot {
            id: "snapshot".to_owned(),
            repository_id: 1,
            request_token: "token".to_owned(),
            config_json: "{}".to_owned(),
            state: "enabled".to_owned(),
            garm_pool_id: Some("pool".to_owned()),
            expires_at: 4_000_000_000,
        })
        .unwrap_or_else(|_| panic!("persist admission snapshot"));
    let (allocation, _) = journal
        .reserve_allocation(allocation_id, "controller", runner_name, "snapshot", "pool")
        .unwrap_or_else(|_| panic!("reserve allocation"));
    assert!(
        journal
            .claim_allocation_selection(&allocation.allocation_id)
            .unwrap_or_else(|_| panic!("claim selection"))
    );
    journal
        .set_allocation_selection(
            &allocation.allocation_id,
            "{}",
            "{}",
            account_scope,
            "catalog-fingerprint",
        )
        .unwrap_or_else(|_| panic!("persist native account scope"));
    journal
        .insert_child_attempt(&ChildAttemptRecord {
            allocation_id: allocation.allocation_id.clone(),
            attempt_number: 1,
            state: "reserved".to_owned(),
            offer_json: "{}".to_owned(),
            binding_json: "{}".to_owned(),
            detail: None,
            created_at: 1,
            dispatched_at: None,
            completed_at: None,
        })
        .unwrap_or_else(|_| panic!("record child attempt"));
    journal
        .mark_child_dispatched(
            &allocation.allocation_id,
            1,
            &outrider::journal::AllocationAdmission {
                snapshot_id: "snapshot".to_owned(),
                repository_id: 1,
                run_id: 1,
                run_attempt: 1,
                now: unix_now_for_test(),
            },
        )
        .unwrap_or_else(|_| panic!("persist dispatch"));
    journal
        .mark_allocation_ambiguous(&allocation.allocation_id, "uncertain")
        .unwrap_or_else(|_| panic!("fence ambiguous allocation"));
}

#[test]
fn same_name_retries_resolve_one_opaque_allocation_and_tombstone() {
    let fixture = JournalFixture::new();
    let journal = fixture.journal();
    let (first, created) = journal
        .reserve_allocation(
            "opaque-allocation-a",
            "controller",
            "runner",
            "snapshot",
            "pool",
        )
        .unwrap_or_else(|_| panic!("reserve allocation"));
    assert!(created);
    assert_eq!(first.state, "reserved");

    let (retry, created) = journal
        .reserve_allocation(
            "opaque-allocation-b",
            "controller",
            "runner",
            "snapshot",
            "pool",
        )
        .unwrap_or_else(|_| panic!("join same-name allocation"));
    assert!(!created);
    assert_eq!(retry.allocation_id, first.allocation_id);

    let by_name = journal
        .allocation("controller", "runner")
        .unwrap_or_else(|_| panic!("lookup by name"))
        .unwrap_or_else(|| panic!("allocation exists"));
    let by_id = journal
        .allocation("controller", "opaque-allocation-a")
        .unwrap_or_else(|_| panic!("lookup by opaque ID"))
        .unwrap_or_else(|| panic!("allocation exists"));
    assert_eq!(by_name.allocation_id, by_id.allocation_id);

    let tombstoned_by_name = journal
        .tombstone_allocation("controller", "runner")
        .unwrap_or_else(|_| panic!("tombstone by name"))
        .unwrap_or_else(|| panic!("allocation exists"));
    let tombstoned_by_id = journal
        .tombstone_allocation("controller", "opaque-allocation-a")
        .unwrap_or_else(|_| panic!("tombstone by opaque ID"))
        .unwrap_or_else(|| panic!("allocation exists"));
    assert!(tombstoned_by_name.tombstone);
    assert_eq!(
        tombstoned_by_name.allocation_id,
        tombstoned_by_id.allocation_id
    );
}

#[test]
fn active_ambiguous_rows_count_against_all_allocation_caps() {
    let fixture = JournalFixture::new();
    let journal = fixture.journal();
    journal
        .save_pool_snapshot(&PoolSnapshot {
            id: "snapshot".to_owned(),
            repository_id: 1,
            request_token: "token".to_owned(),
            config_json: "{}".to_owned(),
            state: "enabled".to_owned(),
            garm_pool_id: Some("pool".to_owned()),
            expires_at: unix_now_for_test() + 3600,
        })
        .unwrap_or_else(|_| panic!("persist admission snapshot"));
    let (allocation, _) = journal
        .reserve_allocation(
            "opaque-allocation",
            "controller",
            "runner",
            "snapshot",
            "pool",
        )
        .unwrap_or_else(|_| panic!("reserve allocation"));
    assert!(
        journal
            .claim_allocation_selection(&allocation.allocation_id)
            .unwrap_or_else(|_| panic!("claim selection"))
    );
    journal
        .set_allocation_selection(
            &allocation.allocation_id,
            "{}",
            "{}",
            "account",
            "catalog-fingerprint",
        )
        .unwrap_or_else(|_| panic!("persist account selection"));
    journal
        .insert_child_attempt(&ChildAttemptRecord {
            allocation_id: allocation.allocation_id.clone(),
            attempt_number: 1,
            state: "reserved".to_owned(),
            offer_json: "{}".to_owned(),
            binding_json: "{}".to_owned(),
            detail: None,
            created_at: 1,
            dispatched_at: None,
            completed_at: None,
        })
        .unwrap_or_else(|_| panic!("record child attempt before dispatch"));
    journal
        .mark_child_dispatched(
            &allocation.allocation_id,
            1,
            &outrider::journal::AllocationAdmission {
                snapshot_id: "snapshot".to_owned(),
                repository_id: 1,
                run_id: 1,
                run_attempt: 1,
                now: unix_now_for_test(),
            },
        )
        .unwrap_or_else(|_| panic!("persist dispatch before child execution"));
    journal
        .mark_allocation_ambiguous(&allocation.allocation_id, "uncertain")
        .unwrap_or_else(|_| panic!("fence uncertain child result"));
    assert_eq!(
        journal
            .active_allocation_counts("controller", "pool")
            .unwrap_or_else(|_| panic!("count controller and pool allocation")),
        (1, 1)
    );
    assert_eq!(
        journal
            .active_allocation_counts_for_account("controller", "pool", "account")
            .unwrap_or_else(|_| panic!("count account-scoped allocation")),
        (1, 1, 1)
    );
    assert_eq!(
        journal
            .active_allocation_counts_for_account("controller", "pool", "other-account")
            .unwrap_or_else(|_| panic!("count another account scope")),
        (1, 1, 0)
    );
}

#[test]
fn azure_subscription_aliases_share_one_native_account_cap() {
    let uppercase = canonical_native_account_scope("azure", "AABBCCDD-0000-1111-2222-333344445555")
        .unwrap_or_else(|_| panic!("normalize Azure subscription scope"));
    let lowercase = canonical_native_account_scope("azure", "aabbccdd-0000-1111-2222-333344445555")
        .unwrap_or_else(|_| panic!("normalize Azure subscription scope"));
    assert_eq!(uppercase, lowercase);

    let fixture = JournalFixture::new();
    let journal = fixture.journal();
    record_ambiguous_account_allocation(&journal, "allocation-a", "runner-a", &uppercase);
    record_ambiguous_account_allocation(&journal, "allocation-b", "runner-b", &lowercase);
    assert_eq!(
        journal
            .active_allocation_counts_for_account("controller", "pool", &uppercase)
            .unwrap_or_else(|_| panic!("count physical subscription scope")),
        (2, 2, 2)
    );
}
