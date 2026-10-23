use super::*;

#[tokio::test]
async fn supplied_pool_conflict_precedes_lifecycle_admission() {
    let harness = AllocationHarness::new(AllocationHarnessOptions::default()).await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap();
    for command in ["StartInstance", "StopInstance", "DeleteInstance"] {
        let mut request = harness.request.clone();
        request.command = command.to_owned();
        request.instance_id = Some("runner-old-image".to_owned());
        request.pool_id = Some("foreign-pool".to_owned());
        request.bootstrap = None;
        request.pool_extra_specs = None;
        assert!(matches!(
            harness.service.handle(request).await,
            Err(AllocationError::Conflict)
        ));
        let allocation = harness.allocation();
        assert!(!allocation.tombstone);
        assert!(
            harness
                .journal
                .allocation_action(&allocation.allocation_id)
                .unwrap()
                .is_none()
        );
    }

    assert!(!harness.lifecycle_commands().iter().any(|command| matches!(
        command.as_str(),
        "StartInstance" | "StopInstance" | "DeleteInstance"
    )));
}

#[tokio::test]
async fn malformed_snapshot_ids_reject_before_reservation() {
    let harness = AllocationHarness::new(AllocationHarnessOptions::default()).await;
    for invalid in [
        json!(null),
        json!(12),
        json!(""),
        json!([]),
        json!({}),
        json!("other-id"),
        json!("  "),
    ] {
        for bootstrap_invalid in [false, true] {
            let mut request = harness.request.clone();
            request.bootstrap.as_mut().unwrap().extra_specs =
                Some(json!({"snapshot_id":"snapshot-id"}));
            if bootstrap_invalid {
                request.bootstrap.as_mut().unwrap().extra_specs =
                    Some(json!({"snapshot_id":invalid}));
            } else {
                request.pool_extra_specs = Some(json!({"snapshot_id":invalid}));
            }

            assert!(matches!(
                harness.service.handle(request).await,
                Err(AllocationError::Snapshot)
            ));
            assert!(
                harness
                    .journal
                    .allocations_for_controller("controller-1")
                    .unwrap()
                    .is_empty()
            );
        }
    }

    assert!(harness.lifecycle_commands().is_empty());
}

#[tokio::test]
async fn closed_pre_spawn_failure_does_not_poison_live_pool_list() {
    let harness = AllocationHarness::new(AllocationHarnessOptions::default()).await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap();
    let (failed, _) = harness
        .journal
        .reserve_allocation(
            "closed-local",
            "controller-1",
            "rejected-runner",
            "snapshot-id",
            "pool-1",
        )
        .unwrap();
    harness
        .journal
        .set_allocation_state(&failed.allocation_id, "no_candidate", None)
        .unwrap();
    let mut list = harness.request.clone();
    list.command = "ListInstances".to_owned();
    list.bootstrap = None;
    list.pool_extra_specs = None;
    let result = harness
        .service
        .handle(list)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "List failed: {error:?}; child lifecycle={:?}; native inspections={}; delayed native inspections={}; allocation state={}",
                harness.lifecycle_commands(),
                harness.native_inspections.load(Ordering::Relaxed),
                harness._delayed_native_inspections.load(Ordering::Relaxed),
                harness.allocation().state,
            )
        })
        .unwrap();
    assert_eq!(result.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn list_native_observation_respects_fixture_provider_budget() {
    let short = AllocationHarness::new(AllocationHarnessOptions {
        provider_timeout_seconds: 1,
        ..AllocationHarnessOptions::default()
    })
    .await;
    short
        .service
        .handle(short.request.clone())
        .await
        .expect("create allocation before delayed List observation");
    short
        ._native_inspection_delay_ms
        .store(1500, Ordering::Relaxed);
    let mut list = short.request.clone();
    list.command = "ListInstances".to_owned();
    list.bootstrap = None;
    list.pool_extra_specs = None;
    let started = tokio::time::Instant::now();
    let short_result = short.service.handle(list).await;
    let short_elapsed = started.elapsed();
    assert!(
        matches!(short_result, Err(AllocationError::Catalog)),
        "one-second provider budget should expire during the controlled 1500ms AWS DescribeInstances response: {short_result:?}; lifecycle={:?}; delayed inspections={}; elapsed={short_elapsed:?}",
        short.lifecycle_commands(),
        short._delayed_native_inspections.load(Ordering::Relaxed),
    );
    assert_eq!(short._delayed_native_inspections.load(Ordering::Relaxed), 1);
    assert!(short_elapsed < Duration::from_millis(1400));

    let long = AllocationHarness::new(AllocationHarnessOptions {
        provider_timeout_seconds: 5,
        ..AllocationHarnessOptions::default()
    })
    .await;
    long.service
        .handle(long.request.clone())
        .await
        .expect("create allocation before delayed List observation");
    long._native_inspection_delay_ms
        .store(1500, Ordering::Relaxed);
    let mut list = long.request.clone();
    list.command = "ListInstances".to_owned();
    list.bootstrap = None;
    list.pool_extra_specs = None;
    let started = tokio::time::Instant::now();
    let long_result = long.service.handle(list).await;
    let long_elapsed = started.elapsed();
    let listed = long_result.unwrap_or_else(|error| {
        panic!(
            "five-second provider budget should allow the controlled AWS DescribeInstances response: {error:?}; lifecycle={:?}; native inspections={}; delayed inspections={}; state={}",
            long.lifecycle_commands(),
            long.native_inspections.load(Ordering::Relaxed),
            long._delayed_native_inspections.load(Ordering::Relaxed),
            long.allocation().state,
        )
    });
    assert_eq!(long._delayed_native_inspections.load(Ordering::Relaxed), 1);
    assert!(long_elapsed >= Duration::from_millis(1500));
    assert_eq!(listed.unwrap().as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn pre_spawn_mapping_failure_releases_caps() {
    let harness = AllocationHarness::new(AllocationHarnessOptions::default()).await;
    let mut request = harness.request.clone();
    request.bootstrap.as_mut().unwrap().metadata_url = "invalid".to_owned();
    assert!(harness.service.handle(request).await.is_err());
    assert_eq!(harness.allocation().state, "no_candidate");
    assert_eq!(
        harness
            .journal
            .active_allocation_counts("controller-1", "pool-1")
            .unwrap(),
        (0, 0)
    );
    assert!(
        !harness
            .lifecycle_commands()
            .contains(&"CreateInstance".to_owned())
    );
}

impl AllocationHarness {
    pub(super) fn lifecycle_request(&self, command: &str, name: &str) -> ProviderRequest {
        ProviderRequest {
            command: command.to_owned(),
            interface_version: "v0.1.0".to_owned(),
            controller_id: "controller-1".to_owned(),
            pool_id: None,
            instance_id: Some(name.to_owned()),
            bootstrap: None,
            pool_extra_specs: None,
        }
    }

    pub(super) async fn wait_for_create_barrier(&self) {
        let marker = self._fixture.directory.join("create-barrier.ready");
        tokio::time::timeout(Duration::from_secs(3), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|error| panic!("real child Create barrier: {error}"));
    }

    pub(super) fn release_create_barrier(&self) {
        fs::write(
            self._fixture.directory.join("create-barrier.release"),
            "release",
        )
        .unwrap_or_else(|error| panic!("allocation fixture: {error}"));
    }
}

#[tokio::test]
async fn positive_create_after_tombstone_uses_guarded_child_cleanup() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        spot: false,
        child_mode: "barrier_create".to_owned(),
        ..AllocationHarnessOptions::default()
    })
    .await;
    harness.native_visible.store(false, Ordering::Relaxed);
    let create = harness.service.handle(harness.request.clone());
    let delete_during_create = async {
        harness.wait_for_create_barrier().await;
        assert!(matches!(
            harness
                .service
                .handle(harness.lifecycle_request("DeleteInstance", "runner-old-image"))
                .await,
            Err(AllocationError::Ambiguous)
        ));
        assert!(harness.allocation().tombstone);
        assert!(
            !harness
                .lifecycle_commands()
                .contains(&"DeleteInstance".to_owned())
        );
        harness.native_visible.store(true, Ordering::Relaxed);
        harness.release_create_barrier();
    };
    let (result, ()) = tokio::join!(create, delete_during_create);
    assert!(matches!(result, Err(AllocationError::Conflict)));
    let allocation = harness.allocation();
    assert_eq!(allocation.state, "deleted");
    assert!(allocation.returned_at.is_some());
    let raw: Value =
        serde_json::from_str(allocation.provider_instance_json.as_deref().unwrap()).unwrap();
    assert_eq!(raw["provider_id"], "i-synthetic-001");
    assert!(allocation.native_identity_json.is_some());
    let action = harness
        .journal
        .allocation_action(&allocation.allocation_id)
        .unwrap()
        .unwrap();
    assert_eq!(action.state, "complete");
    assert!(!action.unresolved_create);
    let records = fs::read_to_string(&harness.lifecycle_log).unwrap();
    let cleanup: Value = records
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["command"] == "DeleteInstance")
        .unwrap();
    assert_eq!(cleanup["instance_id"], "i-synthetic-001");
    assert_eq!(cleanup["controller_id"], "controller-1");
    assert_eq!(cleanup["pool_id"], "pool-1");
}

#[tokio::test]
async fn tombstoned_positive_create_cannot_delete_another_native_claim() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        spot: false,
        child_mode: "barrier_create".to_owned(),
        ..AllocationHarnessOptions::default()
    })
    .await;
    harness.release_create_barrier();
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap();
    fs::remove_file(harness._fixture.directory.join("create-barrier.release")).unwrap();
    fs::remove_file(harness._fixture.directory.join("create-barrier.ready")).unwrap();
    let mut request = harness.request.clone();
    request.bootstrap.as_mut().unwrap().name = "runner-collision".to_owned();
    let create = harness.service.handle(request);
    let delete_during_create = async {
        harness.wait_for_create_barrier().await;
        assert!(
            harness
                .service
                .handle(harness.lifecycle_request("DeleteInstance", "runner-collision"))
                .await
                .is_err()
        );
        harness.release_create_barrier();
    };
    let (result, ()) = tokio::join!(create, delete_during_create);
    assert!(matches!(result, Err(AllocationError::Ambiguous)));
    let collision = harness
        .journal
        .allocation("controller-1", "runner-collision")
        .unwrap()
        .unwrap();
    assert!(collision.tombstone);
    assert!(collision.returned_at.is_some());
    assert!(collision.provider_instance_json.is_some());
    assert!(collision.native_identity_json.is_none());
    assert_eq!(harness.allocation().state, "created");
    assert!(
        !harness
            .lifecycle_commands()
            .contains(&"DeleteInstance".to_owned())
    );
}

#[tokio::test]
async fn child_omission_inspects_native_id_despite_missing_name_inventory() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        spot: false,
        child_mode: "omit_list".to_owned(),
        provider_timeout_seconds: 5,
        ..AllocationHarnessOptions::default()
    })
    .await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap();
    harness.native_visible.store(false, Ordering::Relaxed);
    harness.native_inspection_mode.store(1, Ordering::Relaxed);
    let mut list = harness.lifecycle_request("ListInstances", "runner-old-image");
    list.instance_id = None;
    list.pool_id = Some("pool-1".to_owned());
    let result = harness.service.handle(list.clone()).await.unwrap().unwrap();
    assert_eq!(result.as_array().unwrap().len(), 1);
    assert!(harness.native_inspections.load(Ordering::Relaxed) > 0);
    for mode in [2, 3, 4] {
        let before = harness.native_inspections.load(Ordering::Relaxed);
        harness
            .native_inspection_mode
            .store(mode, Ordering::Relaxed);
        let result = harness.service.handle(list.clone()).await;
        let lifecycle = fs::read_to_string(&harness.lifecycle_log).unwrap_or_default();
        assert!(
            result.is_err(),
            "inspection mode {mode} must not hide an allocation: {result:?}"
        );
        assert!(
            harness.native_inspections.load(Ordering::Relaxed) > before,
            "inspection mode {mode} returned {result:?} without inspecting native state; child lifecycle: {lifecycle}"
        );
    }
}

#[tokio::test]
async fn remove_all_tombstones_ambiguous_and_live_before_independent_effects() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        spot: false,
        child_mode: "mixed_create".to_owned(),
        ..AllocationHarnessOptions::default()
    })
    .await;
    let mut unknown = harness.request.clone();
    unknown.bootstrap.as_mut().unwrap().name = "unknown-create".to_owned();
    assert!(matches!(
        harness.service.handle(unknown).await,
        Err(AllocationError::Ambiguous)
    ));
    let ambiguous = harness
        .journal
        .allocation("controller-1", "unknown-create")
        .unwrap()
        .unwrap();
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap();
    let mut remove = harness.lifecycle_request("RemoveAllInstances", "");
    remove.instance_id = None;
    let result = harness.service.handle(remove).await;
    assert!(matches!(result, Err(AllocationError::Ambiguous)));
    assert!(harness.allocation().tombstone);
    assert_eq!(harness.allocation().state, "deleted");
    let unresolved = harness
        .journal
        .allocation("controller-1", &ambiguous.allocation_id)
        .unwrap()
        .unwrap();
    assert!(unresolved.tombstone);
    assert_eq!(unresolved.state, "deleting");
    assert!(
        harness
            .journal
            .allocation_action(&ambiguous.allocation_id)
            .unwrap()
            .unwrap()
            .state
            != "complete"
    );
    let commands = harness.lifecycle_commands();
    assert!(commands.contains(&"DeleteInstance".to_owned()));
    assert!(!commands.contains(&"RemoveAllInstances".to_owned()));
}
