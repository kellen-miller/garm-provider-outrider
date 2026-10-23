use super::*;

#[tokio::test]
async fn remove_all_independent_children_share_one_operation_deadline() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        spot: false,
        child_mode: "multiple_slow_delete".to_owned(),
        provider_timeout_seconds: 1,
        ..AllocationHarnessOptions::default()
    })
    .await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap();
    let mut second = harness.request.clone();
    second.bootstrap.as_mut().unwrap().name = "runner-second".to_owned();
    harness.service.handle(second).await.unwrap();
    let mut remove = harness.lifecycle_request("RemoveAllInstances", "");
    remove.instance_id = None;
    let started = tokio::time::Instant::now();
    assert!(matches!(
        harness.service.handle(remove).await,
        Err(AllocationError::Ambiguous)
    ));
    assert!(
        started.elapsed() < Duration::from_millis(1800),
        "two slow children must share the original one-second deadline"
    );
    let allocations = harness
        .journal
        .allocations_for_controller("controller-1")
        .unwrap();
    assert_eq!(allocations.len(), 2);
    for allocation in &allocations {
        assert!(allocation.tombstone);
        assert_eq!(allocation.state, "deleting");
        assert_eq!(
            harness
                .journal
                .allocation_action(&allocation.allocation_id)
                .unwrap()
                .unwrap()
                .state,
            "pending"
        );
    }

    let records = fs::read_to_string(&harness.lifecycle_log).unwrap();
    let deletes = records
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record["command"] == "DeleteInstance")
        .collect::<Vec<_>>();
    assert_eq!(
        deletes.len(),
        2,
        "both independent children must receive safe cleanup before the deadline"
    );
    let ids = deletes
        .iter()
        .filter_map(|record| record["instance_id"].as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids, BTreeSet::from(["i-synthetic-001", "i-synthetic-002"]));
    let launch_gap = (deletes[0]["at_monotonic"].as_f64().unwrap()
        - deletes[1]["at_monotonic"].as_f64().unwrap())
    .abs();
    assert!(
        launch_gap < 0.5,
        "independent cleanup effects run concurrently"
    );
}

#[tokio::test]
async fn orphaned_start_completes_from_exact_observed_native_state() {
    let harness = AllocationHarness::new(AllocationHarnessOptions::default()).await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap_or_else(|_| panic!("create healthy runner"));
    harness.native_inspection_mode.store(1, Ordering::Relaxed);
    let allocation = harness.allocation();
    let action = harness
        .journal
        .begin_allocation_action("controller-1", &allocation.allocation_id, "StartInstance")
        .unwrap_or_else(|_| panic!("record interrupted start"))
        .unwrap_or_else(|| panic!("healthy allocation"));
    assert!(action.acquired);
    harness
        .journal
        .recover_startup_allocations()
        .unwrap_or_else(|_| panic!("mark startup orphan"));
    assert_eq!(
        harness
            .service
            .recover_orphaned_lifecycle_actions(1)
            .await
            .unwrap_or_else(|_| panic!("reconcile exact running state")),
        1
    );
    let recovered = harness
        .journal
        .allocation_action(&allocation.allocation_id)
        .unwrap_or_else(|_| panic!("read recovered start"))
        .unwrap_or_else(|| panic!("recovered start"));
    assert_eq!(recovered.state, "complete");
    assert_eq!(harness.allocation().state, "created");
    assert!(!harness.allocation().tombstone);
    assert!(
        !harness
            .lifecycle_commands()
            .contains(&"StartInstance".to_owned())
    );
}

#[tokio::test]
async fn orphaned_start_outage_or_replacement_stays_ambiguous() {
    for inspection_mode in [2, 3, 4] {
        let harness = AllocationHarness::new(AllocationHarnessOptions::default()).await;
        harness
            .service
            .handle(harness.request.clone())
            .await
            .unwrap_or_else(|_| panic!("create healthy runner"));
        let allocation = harness.allocation();
        harness
            .journal
            .begin_allocation_action("controller-1", &allocation.allocation_id, "StartInstance")
            .unwrap_or_else(|_| panic!("record interrupted start"));
        harness
            .journal
            .recover_startup_allocations()
            .unwrap_or_else(|_| panic!("mark startup orphan"));
        harness
            .native_inspection_mode
            .store(inspection_mode, Ordering::Relaxed);
        assert!(matches!(
            harness.service.recover_orphaned_lifecycle_actions(1).await,
            Err(AllocationError::Ambiguous | AllocationError::Catalog)
        ));
        let recovered = harness
            .journal
            .allocation_action(&allocation.allocation_id)
            .unwrap_or_else(|_| panic!("read unresolved start"))
            .unwrap_or_else(|| panic!("unresolved start"));
        assert_eq!(recovered.state, "ambiguous");
        assert_eq!(
            recovered.detail.as_deref(),
            Some("restart lifecycle outcome could not be verified")
        );
        assert_eq!(harness.allocation().state, "created");
        assert!(!harness.allocation().tombstone);
    }
}

#[tokio::test]
async fn allocation_contract_reuses_restart_cleanup_fixture() {
    allocation_harness::restart_retained_cleanup_case().await;
}

#[tokio::test]
async fn allocation_contract_reuses_retained_positive_absence_fixture() {
    allocation_harness::restart_retained_positive_absence_case().await;
}

#[tokio::test]
async fn cleanup_poison_does_not_starve_orphaned_lifecycle_rows() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        spot: false,
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
    let mut poison = harness.request.clone();
    poison
        .bootstrap
        .as_mut()
        .unwrap_or_else(|| panic!("poison bootstrap"))
        .name = "runner-poison".to_owned();
    harness
        .service
        .handle(poison)
        .await
        .unwrap_or_else(|_| panic!("create poison runner"));
    harness.native_inspection_mode.store(1, Ordering::Relaxed);
    let allocations = harness
        .journal
        .allocations_for_controller("controller-1")
        .unwrap_or_else(|_| panic!("read lifecycle fixtures"));
    let poison = allocations
        .iter()
        .find(|allocation| allocation.runner_name == "runner-old-image")
        .unwrap_or_else(|| panic!("poison allocation"));
    harness
        .journal
        .schedule_allocation_cleanup(&poison.allocation_id, "poisoned cleanup fixture")
        .unwrap_or_else(|_| panic!("persist poison cleanup"));
    for allocation in allocations
        .iter()
        .filter(|allocation| allocation.runner_name != "runner-old-image")
    {
        harness
            .journal
            .begin_allocation_action("controller-1", &allocation.allocation_id, "StartInstance")
            .unwrap_or_else(|_| panic!("record interrupted start"));
    }
    harness
        .journal
        .recover_startup_allocations()
        .unwrap_or_else(|_| panic!("mark interrupted actions ambiguous"));

    for attempt in 0..2 {
        let cleanup = harness.service.recover_pending_cleanup(1).await;
        assert!(if attempt == 0 {
            matches!(cleanup, Err(AllocationError::Ambiguous))
        } else {
            matches!(cleanup, Ok(0))
        });
        if attempt == 0 {
            let connection =
                rusqlite::Connection::open(harness._fixture.directory.join("journal.sqlite3"))
                    .expect("open poison cleanup journal");
            connection
                .execute(
                    "UPDATE allocation_actions SET next_attempt_at=CAST(strftime('%s','now') AS INTEGER)+300 WHERE allocation_id=?1 AND reusable=1",
                    [&poison.allocation_id],
                )
                .expect("keep first poison cleanup not due");
            drop(connection);
        }
        assert!(matches!(
            harness.service.recover_orphaned_lifecycle_actions(1).await,
            Ok(1)
        ));
    }
    let actions = harness
        .journal
        .recoverable_lifecycle_actions()
        .unwrap_or_else(|_| panic!("read remaining orphaned actions"));
    assert!(
        actions.is_empty(),
        "both lifecycle rows progress past poison"
    );
    let poison = harness
        .journal
        .allocation("controller-1", "runner-old-image")
        .unwrap_or_else(|_| panic!("read poison allocation"))
        .unwrap_or_else(|| panic!("poison allocation exists"));
    assert_eq!(poison.state, "deleting");
    assert!(poison.tombstone);
}

#[tokio::test]
async fn v010_omitted_pool_uses_retained_scope_for_each_mutation() {
    for command in ["StartInstance", "StopInstance", "DeleteInstance"] {
        let harness = AllocationHarness::new(AllocationHarnessOptions::default()).await;
        harness
            .service
            .handle(harness.request.clone())
            .await
            .unwrap();
        let result = harness
            .service
            .handle(harness.lifecycle_request(command, "runner-old-image"))
            .await;
        assert!(!matches!(result, Err(AllocationError::Conflict)));
        let records = fs::read_to_string(&harness.lifecycle_log).unwrap();
        let action: Value = records
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|record| record["command"] == command)
            .unwrap();
        assert_eq!(action["instance_id"], "i-synthetic-001");
        assert_eq!(action["pool_id"], "pool-1");
        assert_eq!(action["controller_id"], "controller-1");
    }
}

#[tokio::test]
async fn positive_child_return_survives_unknown_native_resolution() {
    let harness = AllocationHarness::new(AllocationHarnessOptions::default()).await;
    harness.native_visible.store(false, Ordering::Relaxed);
    assert!(matches!(
        harness.service.handle(harness.request.clone()).await,
        Err(AllocationError::Ambiguous)
    ));
    let allocation = harness.allocation();
    assert!(allocation.returned_at.is_some());
    assert!(allocation.native_identity_json.is_none());
    let response: Value =
        serde_json::from_str(allocation.provider_instance_json.as_deref().unwrap()).unwrap();
    assert_eq!(response["provider_id"], "i-synthetic-001");
    assert_eq!(
        harness
            .journal
            .child_attempts(&allocation.allocation_id)
            .unwrap()[0]
            .state,
        "created"
    );
    assert!(
        harness
            .service
            .handle(harness.lifecycle_request("DeleteInstance", "runner-old-image"))
            .await
            .is_err()
    );
    let current = harness.allocation();
    assert!(current.tombstone);
    assert_eq!(
        current.provider_instance_json,
        allocation.provider_instance_json
    );
    assert_eq!(
        harness
            .journal
            .allocation_action(&allocation.allocation_id)
            .unwrap()
            .unwrap()
            .state,
        "pending"
    );
    assert!(
        !harness
            .lifecycle_commands()
            .contains(&"DeleteInstance".to_owned())
    );
}
