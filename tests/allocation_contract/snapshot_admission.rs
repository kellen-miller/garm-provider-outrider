use super::*;

#[tokio::test]
async fn allocation_uses_retained_snapshot_image_and_current_limits() {
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
    let bootstrap = harness
        .request
        .bootstrap
        .as_mut()
        .unwrap_or_else(|| panic!("bootstrap"));
    bootstrap.jit_config_enabled = true;
    bootstrap.labels.clear();
    bootstrap.extra_specs.get_or_insert_with(|| json!({}))["runner_install_template"] =
        json!(STANDARD.encode("incoming-wrapper-must-not-execute"));
    let created = Box::pin(harness.service.handle(harness.request.clone()))
        .await
        .unwrap_or_else(|_| panic!("allocation using retained snapshot catalog"));
    assert!(created.is_some());
    let record: Value = serde_json::from_slice(
        &fs::read(&harness.record_path).unwrap_or_else(|_| panic!("provider record")),
    )
    .unwrap_or_else(|_| panic!("recorded child request"));
    let raw = STANDARD
        .decode(
            record["raw"]
                .as_str()
                .unwrap_or_else(|| panic!("base64 child request")),
        )
        .unwrap_or_else(|_| panic!("decode child request"));
    let request: Value =
        serde_json::from_slice(&raw).unwrap_or_else(|_| panic!("decode child request JSON"));
    assert_eq!(request["image"], "oldAMI");
    assert_eq!(request["labels"], json!([]));
    let template = STANDARD
        .decode(
            request["extra_specs"]["runner_install_template"]
                .as_str()
                .expect("generated child template"),
        )
        .expect("decode generated template");
    assert!(!String::from_utf8_lossy(&template).contains("incoming-wrapper-must-not-execute"));
    assert_eq!(harness.allocation().state, "created");

    let denied_harness = AllocationHarness::new(AllocationHarnessOptions {
        current_max_vcpu: 4,
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
    let denied = Box::pin(
        denied_harness
            .service
            .handle(denied_harness.request.clone()),
    )
    .await;
    assert!(
        denied.is_err(),
        "current hard limit must reject the retained offer"
    );
    assert!(
        !denied_harness.record_path.exists(),
        "a rejected offer must not spawn the child"
    );
}

#[tokio::test]
async fn wrapper_replacement_keeps_other_bootstrap_restrictions() {
    for (field, value) in [
        ("pre_install_scripts", json!({"linux": "echo forbidden"})),
        ("extra_context", json!({"forbidden": "value"})),
        ("unapproved_extra", json!("forbidden")),
    ] {
        let mut harness = AllocationHarness::new(AllocationHarnessOptions::default()).await;
        let bootstrap = harness.request.bootstrap.as_mut().expect("bootstrap");
        let extras = bootstrap.extra_specs.get_or_insert_with(|| json!({}));
        extras["runner_install_template"] = json!(STANDARD.encode("incoming-wrapper"));
        extras[field] = value;
        let result = harness.service.handle(harness.request.clone()).await;
        assert!(
            matches!(result, Err(AllocationError::NoCandidate)),
            "{field}: {result:?}"
        );
        assert!(
            !harness.record_path.exists(),
            "{field} must not reach the child"
        );
        assert!(harness.lifecycle_commands().is_empty());
    }
}

#[tokio::test]
async fn absent_delete_fences_delayed_create_before_child_dispatch() {
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
    let mut delete = harness.request.clone();
    delete.command = "DeleteInstance".to_owned();
    delete.instance_id = Some("runner-old-image".to_owned());
    delete.bootstrap = None;
    delete.pool_extra_specs = None;
    assert!(matches!(
        harness.service.handle(delete).await,
        Err(AllocationError::NotFound)
    ));
    assert!(matches!(
        harness.service.handle(harness.request.clone()).await,
        Err(AllocationError::DeletedName)
    ));
    assert!(
        !harness.record_path.exists(),
        "late create must be stopped before child dispatch"
    );
    assert!(
        harness
            .journal
            .allocation("controller-1", "runner-old-image")
            .expect("read allocation after absent delete")
            .is_none()
    );

    drop(harness.service.0.take());
    drop(harness.journal.0.take());
    let journal = Arc::new(harness._fixture.journal());
    let service = rebuild_allocation_service(
        &harness._fixture,
        Arc::clone(&journal),
        &harness.aws_endpoint,
        &harness.old_config,
        &harness.current_config,
        "record",
    )
    .await;
    assert!(matches!(
        service.handle(harness.request.clone()).await,
        Err(AllocationError::DeletedName)
    ));
    assert!(
        !harness.record_path.exists(),
        "reopened service must reject before child dispatch"
    );
}

#[tokio::test]
async fn compacted_snapshot_does_not_block_delete_of_unspawned_allocation() {
    let mut harness = AllocationHarness::new(AllocationHarnessOptions {
        child_mode: "record".to_owned(),
        ..AllocationHarnessOptions::default()
    })
    .await;
    let (allocation, created) = harness
        .journal
        .reserve_allocation(
            "allocation-compacted-delete",
            "controller-1",
            "runner-old-image",
            "snapshot-id",
            "pool-1",
        )
        .expect("reserve unspawned allocation");
    assert!(created);
    harness
        .journal
        .close_unspawned_allocation(&allocation.allocation_id)
        .expect("close allocation before dispatch");
    harness
        .journal
        .set_pool_snapshot_state("snapshot-id", "deleted", None)
        .expect("terminalize snapshot");
    drop(harness.service.0.take());
    drop(harness.journal.0.take());
    let connection = rusqlite::Connection::open(harness._fixture.directory.join("journal.sqlite3"))
        .expect("age terminal snapshot");
    connection
        .execute(
            "UPDATE pool_snapshots SET terminal_at=1 WHERE id='snapshot-id'",
            [],
        )
        .expect("age terminal snapshot");
    drop(connection);
    let journal = Arc::new(harness._fixture.journal());
    assert!(
        journal
            .maintain_retention(0, 32)
            .expect("compact terminal snapshot")
            > 0
    );
    assert!(
        journal
            .pool_snapshot_by_id("snapshot-id")
            .expect("read compacted snapshot")
            .expect("snapshot exists")
            .config_json
            .contains("compacted")
    );
    let service = Box::pin(rebuild_allocation_service(
        &harness._fixture,
        journal.clone(),
        &harness.aws_endpoint,
        &harness.old_config,
        &harness.current_config,
        "record",
    ))
    .await;
    harness.journal.0 = Some(journal.clone());
    harness.service.0 = Some(service);

    let deleted = harness
        .service
        .handle(harness.lifecycle_request("DeleteInstance", "runner-old-image"))
        .await;
    assert!(
        deleted.is_ok(),
        "terminal no-spawn delete should need no snapshot: {deleted:?}"
    );
    assert!(!harness.record_path.exists(), "no child command should run");
    let delayed_create = harness.service.handle(harness.request.clone()).await;
    assert!(
        matches!(delayed_create, Err(AllocationError::Snapshot)),
        "late create must reject the payload-free snapshot: {delayed_create:?}"
    );
    assert!(!harness.record_path.exists(), "late create stays fenced");
    let (retained_name, created) = harness
        .journal
        .reserve_allocation(
            "allocation-late-create",
            "controller-1",
            "runner-old-image",
            "snapshot-id",
            "pool-1",
        )
        .expect("read same-name delete tombstone");
    assert!(!created);
    assert!(retained_name.tombstone);
    assert_eq!(retained_name.state, "deleted");
    assert_eq!(
        harness
            .journal
            .allocation("controller-1", "runner-old-image")
            .expect("read delete tombstone")
            .expect("retained allocation")
            .state,
        "deleted"
    );
}

#[tokio::test]
async fn reservation_first_resolves_owned_then_uses_normal_delete() {
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
        .expect("create allocated runner");
    let target = harness
        .journal
        .allocation_delete_target("controller-1", "runner-old-image", Some("pool-1"))
        .expect("resolve previously reserved allocation");
    assert!(matches!(
        target,
        outrider::journal::AllocationDeleteTarget::Owned(_)
    ));
    let connection = rusqlite::Connection::open(harness._fixture.directory.join("journal.sqlite3"))
        .expect("open allocation journal for fence check");
    let fences: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM allocation_delete_fences WHERE controller_id=?1 AND identity_key=?2",
            rusqlite::params!["controller-1", "runner-old-image"],
            |row| row.get(0),
        )
        .expect("count absent-delete fences");
    assert_eq!(fences, 0, "owned delete must not create an absence fence");

    let mut delete = harness.request.clone();
    delete.command = "DeleteInstance".to_owned();
    delete.instance_id = Some("runner-old-image".to_owned());
    delete.bootstrap = None;
    delete.pool_extra_specs = None;
    harness
        .service
        .handle(delete)
        .await
        .expect("delete through owned allocation lifecycle");
    let allocation = harness.allocation();
    assert_eq!(allocation.state, "deleted");
    assert!(allocation.tombstone);
    assert!(
        harness
            .lifecycle_commands()
            .contains(&"DeleteInstance".to_owned())
    );
}

#[tokio::test]
async fn generic_child_failure_no_fallback() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        current_max_vcpu: 32,
        current_account_allowed: true,
        max_allocations_account: 100,
        snapshot_state: "enabled".to_owned(),
        expires_in_seconds: 3600,
        allocation_timeout_seconds: 600,
        provider_timeout_seconds: 5,
        spot: false,
        child_mode: "exit1".to_owned(),
    })
    .await;
    let result = Box::pin(harness.service.handle(harness.request.clone())).await;
    assert!(matches!(result, Err(AllocationError::Ambiguous)));
    assert_eq!(harness.allocation().state, "deleting");
    assert!(harness.allocation().tombstone);
    assert!(
        harness.compute_quote_requests.load(Ordering::Relaxed) >= 6,
        "two retained real catalog bindings must quote eligible candidates before generic child failure"
    );
    assert_eq!(
        harness
            .lifecycle_commands()
            .iter()
            .filter(|command| command.as_str() == "CreateInstance")
            .count(),
        1
    );
}

#[tokio::test]
async fn v010_bootstrap_snapshot_reaches_allocation_admission() {
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
    let mut request = harness.request.clone();
    request.pool_extra_specs = None;
    request
        .bootstrap
        .as_mut()
        .unwrap_or_else(|| panic!("bootstrap"))
        .extra_specs = Some(json!({
        "snapshot_id":"snapshot-id"
    }));
    let result = harness.service.handle(request).await;
    assert!(result.is_ok(), "unexpected allocation response: {result:?}");
    assert_eq!(harness.allocation().state, "created");
}

#[tokio::test]
async fn ambiguous_no_duplicate() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        current_max_vcpu: 32,
        current_account_allowed: true,
        max_allocations_account: 100,
        snapshot_state: "enabled".to_owned(),
        expires_in_seconds: 3600,
        allocation_timeout_seconds: 600,
        provider_timeout_seconds: 5,
        spot: false,
        child_mode: "malformed".to_owned(),
    })
    .await;
    let first = Box::pin(harness.service.handle(harness.request.clone())).await;
    let retry = Box::pin(harness.service.handle(harness.request.clone())).await;
    assert!(matches!(first, Err(AllocationError::Ambiguous)));
    assert!(matches!(retry, Err(AllocationError::Ambiguous)));
    assert_eq!(
        harness
            .lifecycle_commands()
            .iter()
            .filter(|command| command.as_str() == "CreateInstance")
            .count(),
        1
    );
}

#[tokio::test]
async fn expired_before_admission_rejected() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        current_max_vcpu: 32,
        current_account_allowed: true,
        max_allocations_account: 100,
        snapshot_state: "enabled".to_owned(),
        expires_in_seconds: -1,
        allocation_timeout_seconds: 600,
        provider_timeout_seconds: 5,
        spot: false,
        child_mode: "record".to_owned(),
    })
    .await;
    assert!(
        harness
            .service
            .handle(harness.request.clone())
            .await
            .is_err()
    );
    assert!(
        harness
            .journal
            .allocation("controller-1", "runner-old-image")
            .unwrap_or_else(|_| panic!("allocation lookup"))
            .is_none()
    );
    assert!(harness.lifecycle_commands().is_empty());
}

#[tokio::test]
async fn expired_nonterminal_fenced() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        current_max_vcpu: 32,
        current_account_allowed: true,
        max_allocations_account: 100,
        snapshot_state: "enabled".to_owned(),
        expires_in_seconds: 5,
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
    assert_eq!(harness.allocation().state, "deleting");
    tokio::time::sleep(Duration::from_millis(5100)).await;
    assert!(matches!(
        harness.service.handle(harness.request.clone()).await,
        Err(AllocationError::Snapshot)
    ));
    assert_eq!(harness.allocation().state, "deleting");
    assert_eq!(
        harness
            .lifecycle_commands()
            .iter()
            .filter(|command| command.as_str() == "CreateInstance")
            .count(),
        1
    );
    assert_eq!(
        harness
            .journal
            .active_allocation_counts("controller-1", "pool-1")
            .unwrap_or_else(|_| panic!("active caps")),
        (1, 1)
    );
}

#[tokio::test]
async fn killed_provider_late_create_delete() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        current_max_vcpu: 32,
        current_account_allowed: true,
        max_allocations_account: 100,
        snapshot_state: "enabled".to_owned(),
        expires_in_seconds: 3600,
        allocation_timeout_seconds: 31,
        provider_timeout_seconds: 1,
        spot: false,
        child_mode: "no_stdin_reader".to_owned(),
    })
    .await;
    assert!(matches!(
        harness.service.handle(harness.request.clone()).await,
        Err(AllocationError::Ambiguous)
    ));
    assert_eq!(harness.allocation().state, "deleting");
    assert!(
        !harness.native_visible.load(Ordering::Relaxed),
        "the native VM must be absent when the provider process is killed"
    );
    harness.native_visible.store(true, Ordering::Relaxed);
    let mut delete = harness.request.clone();
    delete.command = "DeleteInstance".to_owned();
    delete.instance_id = Some("runner-old-image".to_owned());
    delete.bootstrap = None;
    delete.pool_extra_specs = None;
    assert!(matches!(
        harness.service.handle(delete).await,
        Err(AllocationError::Ambiguous)
    ));
    let allocation = harness.allocation();
    assert_eq!(
        allocation.state, "deleting",
        "timed-out cleanup must retain its ownership fence"
    );
    let records =
        fs::read_to_string(&harness.lifecycle_log).unwrap_or_else(|_| panic!("lifecycle records"));
    let delete_record = records
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["command"] == "DeleteInstance")
        .unwrap_or_else(|| panic!("late-create cleanup request"));
    assert_eq!(delete_record["instance_id"], "i-synthetic-001");
}

#[tokio::test]
async fn tightened_limits_and_removed_account() {
    for (max_vcpu, account_allowed) in [(4, true), (32, false)] {
        let harness = AllocationHarness::new(AllocationHarnessOptions {
            current_max_vcpu: max_vcpu,
            current_account_allowed: account_allowed,
            max_allocations_account: 100,
            snapshot_state: "enabled".to_owned(),
            expires_in_seconds: 3600,
            allocation_timeout_seconds: 600,
            provider_timeout_seconds: 5,
            spot: false,
            child_mode: "record".to_owned(),
        })
        .await;
        assert!(
            harness
                .service
                .handle(harness.request.clone())
                .await
                .is_err()
        );
        assert!(!harness.record_path.exists());
        assert!(harness.lifecycle_commands().is_empty());
    }
}
