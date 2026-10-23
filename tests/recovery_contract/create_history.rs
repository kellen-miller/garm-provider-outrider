use super::*;

#[tokio::test]
async fn unknown_create_coverage_survives_restart() {
    let mut harness = continuity_history_harness().await;
    let source_allocation = harness.allocation();
    let source_attempt = harness
        .journal
        .child_attempts(&source_allocation.allocation_id)
        .unwrap_or_else(|_| panic!("read source child attempt"))
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("source child attempt"));
    let catalog_fingerprint = source_allocation
        .catalog_fingerprint
        .as_deref()
        .unwrap_or_else(|| panic!("source catalog fingerprint"));
    let account_id = source_allocation
        .account_id
        .as_deref()
        .unwrap_or("synthetic-account");
    persist_dispatched_history_attempt(
        &harness.journal,
        "allocation-unknown-history",
        "runner-history-unknown",
        &source_attempt.offer_json,
        &source_attempt.binding_json,
        account_id,
        catalog_fingerprint,
    );
    harness
        .journal
        .finish_child_attempt(
            "allocation-unknown-history",
            1,
            "ambiguous",
            "opaque child create result",
        )
        .unwrap_or_else(|_| panic!("persist ambiguous child result"));
    let dispatched_at = harness
        .journal
        .child_attempts("allocation-unknown-history")
        .unwrap_or_else(|_| panic!("read ambiguous dispatch"))[0]
        .dispatched_at
        .unwrap_or_else(|| panic!("ambiguous dispatch timestamp"));
    let overlapping_second = harness
        .journal
        .unknown_create_attempts(
            u128::from(dispatched_at) * 1000 + 200,
            u128::from(dispatched_at) * 1000 + 800,
        )
        .unwrap_or_else(|_| panic!("read overlapping unknown dispatch"));
    assert_eq!(overlapping_second.len(), 1);
    persist_dispatched_history_attempt(
        &harness.journal,
        "allocation-positive-unknown-history",
        "runner-history-positive-unknown",
        &source_attempt.offer_json,
        &source_attempt.binding_json,
        account_id,
        catalog_fingerprint,
    );
    harness
        .journal
        .finish_child_create(
            1,
            &CreateHistoryRecord {
                allocation_id: "allocation-positive-unknown-history".to_owned(),
                candidate_key: "positive-unverified-history".to_owned(),
                catalog_fingerprint: source_allocation
                    .catalog_fingerprint
                    .clone()
                    .unwrap_or_else(|| panic!("source catalog fingerprint")),
                provider_pin_json: "{}".to_owned(),
                observed_at_millis: u128::from(dispatched_at) * 1000 + 700,
                latency_nanos: Some(1),
            },
        )
        .unwrap_or_else(|_| panic!("persist unresolved positive create"));
    harness
        .journal
        .finish_child_attempt(
            "allocation-positive-unknown-history",
            1,
            "ambiguous",
            "native identity collision after positive child return",
        )
        .unwrap_or_else(|_| panic!("retain positive native identity ambiguity"));
    let overlapping_positive = harness
        .journal
        .unknown_create_attempts(
            u128::from(dispatched_at) * 1000 + 500,
            u128::from(dispatched_at) * 1000 + 900,
        )
        .unwrap_or_else(|_| panic!("read unresolved positive in observation window"));
    assert_eq!(overlapping_positive.len(), 2);
    let unresolved_positive_history = harness
        .journal
        .candidate_history(
            "positive-unverified-history",
            catalog_fingerprint,
            "{}",
            u128::from(dispatched_at) * 1000 + 500,
            u128::from(dispatched_at) * 1000 + 900,
            1000,
        )
        .unwrap_or_else(|_| panic!("read unresolved positive metrics"));
    assert_eq!(unresolved_positive_history.definitive_create_outcomes, 0);
    assert_eq!(unresolved_positive_history.measured_latency_samples, 1);
    assert_eq!(unresolved_positive_history.create_latencies_nanos, vec![1]);
    assert_eq!(unresolved_positive_history.latest_create_millis, None);
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
    assert!(harness.service.handle(request).await.is_err());
}

#[tokio::test]
async fn opaque_create_failure_blocks_continuity_after_restart() {
    let mut harness = continuity_history_harness().await;
    let source_allocation = harness.allocation();
    let source_attempt = harness
        .journal
        .child_attempts(&source_allocation.allocation_id)
        .unwrap_or_else(|_| panic!("read successful source attempt"))
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("successful source attempt"));
    drop(
        harness
            .service
            .0
            .take()
            .unwrap_or_else(|| panic!("running allocation service")),
    );
    let journal = harness
        .journal
        .0
        .as_ref()
        .unwrap_or_else(|| panic!("running journal"))
        .clone();
    let failing_service = Box::pin(allocation_harness::rebuild_allocation_service(
        &harness._fixture,
        journal,
        &harness.aws_endpoint,
        &harness.old_config,
        &harness.current_config,
        "exit1",
    ))
    .await;
    harness.service.0 = Some(failing_service);

    let failed_name = "runner-opaque-create";
    let mut failed_request = harness.request.clone();
    set_history_request(
        &mut failed_request,
        failed_name,
        "history-token",
        "snapshot-history",
    );
    let failed_result = harness.service.handle(failed_request).await;
    assert!(
        matches!(
            failed_result,
            Err(outrider::allocation::AllocationError::Ambiguous)
        ),
        "opaque Create must reach the child and fail opaquely: {failed_result:?}"
    );
    let failed_allocation = harness
        .journal
        .allocation("controller-1", failed_name)
        .unwrap_or_else(|_| panic!("read opaque create allocation"))
        .unwrap_or_else(|| panic!("opaque create allocation exists"));
    let failed_attempts = harness
        .journal
        .child_attempts(&failed_allocation.allocation_id)
        .unwrap_or_else(|_| panic!("read opaque create attempt"));
    assert_eq!(failed_attempts.len(), 1);
    assert_eq!(failed_attempts[0].state, "ambiguous");
    assert!(failed_attempts[0].dispatched_at.is_some());
    assert_eq!(
        failed_allocation.catalog_fingerprint,
        Some(
            source_allocation
                .catalog_fingerprint
                .clone()
                .unwrap_or_else(|| panic!("source catalog fingerprint"))
        )
    );
    assert_eq!(failed_attempts[0].binding_json, source_attempt.binding_json);
    let source_offer: outrider::policy::CatalogOffer =
        serde_json::from_str(&source_attempt.offer_json)
            .unwrap_or_else(|_| panic!("parse source offer"));
    let failed_offer: outrider::policy::CatalogOffer =
        serde_json::from_str(&failed_attempts[0].offer_json)
            .unwrap_or_else(|_| panic!("parse failed offer"));
    assert_eq!(failed_offer.selection, source_offer.selection);
    assert_eq!(failed_offer.launch_facts, source_offer.launch_facts);
    assert_eq!(failed_offer.candidate.image, source_offer.candidate.image);
    assert_eq!(
        failed_offer.candidate.machine_type,
        source_offer.candidate.machine_type
    );

    let create_commands_before_reopen = harness
        .lifecycle_commands()
        .into_iter()
        .filter(|command| command == "CreateInstance")
        .count();
    assert!(create_commands_before_reopen >= 2);
    drop(
        harness
            .service
            .0
            .take()
            .unwrap_or_else(|| panic!("failing allocation service")),
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

    let mut ranked_request = harness.request.clone();
    set_history_request(
        &mut ranked_request,
        "runner-after-opaque-create",
        "history-token",
        "snapshot-history",
    );
    assert!(matches!(
        harness.service.handle(ranked_request).await,
        Err(outrider::allocation::AllocationError::NoCandidate)
    ));
    assert_eq!(
        harness
            .lifecycle_commands()
            .into_iter()
            .filter(|command| command == "CreateInstance")
            .count(),
        create_commands_before_reopen
    );

    let mut control = continuity_history_harness().await;
    drop(
        control
            .service
            .0
            .take()
            .unwrap_or_else(|| panic!("control allocation service")),
    );
    drop(
        control
            .journal
            .0
            .take()
            .unwrap_or_else(|| panic!("control journal")),
    );
    let reopened = std::sync::Arc::new(control._fixture.journal());
    let service = Box::pin(allocation_harness::rebuild_allocation_service(
        &control._fixture,
        reopened.clone(),
        &control.aws_endpoint,
        &control.old_config,
        &control.current_config,
        "multiple_slow_delete",
    ))
    .await;
    control.journal.0 = Some(reopened);
    control.service.0 = Some(service);
    let mut known_success_request = control.request.clone();
    set_history_request(
        &mut known_success_request,
        "runner-second",
        "history-token",
        "snapshot-history",
    );
    control
        .service
        .handle(known_success_request)
        .await
        .unwrap_or_else(|error| panic!("known-success continuity control: {error}"));
}

#[tokio::test]
async fn production_failed_create_inspects_and_resolves_through_offline_cli() {
    let mut harness =
        allocation_harness::AllocationHarness::new(allocation_harness::AllocationHarnessOptions {
            child_mode: "create_failure".to_owned(),
            ..allocation_harness::AllocationHarnessOptions::default()
        })
        .await;
    assert!(matches!(
        harness.service.handle(harness.request.clone()).await,
        Err(outrider::allocation::AllocationError::Ambiguous)
    ));
    let allocation = harness.allocation();
    let database = harness._fixture.directory.join("journal.sqlite3");
    harness.server_task.abort();
    drop(harness.service.0.take());
    drop(harness.journal.0.take());

    let inspect = Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args([
            "cleanup",
            "inspect",
            "--database",
            database.to_str().unwrap_or_default(),
            "--controller",
            &allocation.controller_id,
            "--allocation",
            &allocation.allocation_id,
        ])
        .output()
        .unwrap_or_else(|error| panic!("run cleanup inspect: {error}"));
    assert!(
        inspect.status.success(),
        "{}",
        String::from_utf8_lossy(&inspect.stderr)
    );
    let inspection: serde_json::Value = serde_json::from_slice(&inspect.stdout)
        .unwrap_or_else(|error| panic!("parse cleanup inspection: {error}"));
    assert_eq!(inspection["allocation_state"], "deleting");
    assert_eq!(inspection["native_location"], "us-east-1");
    assert!(inspection["cleanup_attempt_count"].as_u64().unwrap_or(1) == 0);

    let proof_path = harness._fixture.directory.join("cleanup-attestation.json");
    let proof = serde_json::json!({
        "schema_version": 1,
        "allocation_id": inspection["allocation_id"],
        "controller_id": inspection["controller_id"],
        "pool_id": inspection["pool_id"],
        "runner_name": inspection["runner_name"],
        "scope_digest": inspection["scope_digest"],
        "evidence_reference": "ticket:INC-482#evidence-7",
        "verified_at_unix_seconds": unix_now(),
        "acknowledgements": {
            "all_cloud_operations_terminal": true,
            "no_local_provider_process_can_submit_work": true,
            "all_resources_and_dependencies_proved_absent": true
        }
    });
    fs::write(
        &proof_path,
        serde_json::to_vec(&proof)
            .unwrap_or_else(|error| panic!("serialize cleanup proof: {error}")),
    )
    .unwrap_or_else(|error| panic!("write cleanup proof: {error}"));
    let resolve = Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args([
            "cleanup",
            "resolve",
            "--database",
            database.to_str().unwrap_or_default(),
            "--attestation",
            proof_path.to_str().unwrap_or_default(),
        ])
        .output()
        .unwrap_or_else(|error| panic!("run cleanup resolve: {error}"));
    assert!(
        resolve.status.success(),
        "{}",
        String::from_utf8_lossy(&resolve.stderr)
    );

    let journal = harness._fixture.journal();
    assert_eq!(
        journal
            .allocation_by_id(&allocation.allocation_id)
            .unwrap_or_else(|error| panic!("read resolved production allocation: {error}"))
            .map(|record| record.state),
        Some("deleted".to_owned())
    );
    assert_eq!(
        journal
            .child_attempts(&allocation.allocation_id)
            .unwrap_or_else(|error| panic!("retain failed child history: {error}"))[0]
            .state,
        "ambiguous"
    );
}
