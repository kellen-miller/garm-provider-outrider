use super::*;

/// Checks that retries after the first delete intent exclude older interruption events.
///
/// # Panics
/// Panics when the local allocation fixture fails an assertion or setup step.
pub async fn first_delete_retry_excludes_interruption() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        spot: true,
        ..AllocationHarnessOptions::default()
    })
    .await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap_or_else(|_| panic!("create Spot runner through pinned child"));
    let allocation = harness.allocation();
    harness
        .journal
        .schedule_allocation_cleanup(&allocation.allocation_id, "first planned delete")
        .unwrap_or_else(|_| panic!("persist first delete intent"));
    let first_delete = harness
        .journal
        .first_allocation_delete_intent(&allocation.allocation_id)
        .unwrap_or_else(|_| panic!("read first delete intent"))
        .unwrap_or_else(|| panic!("first delete intent is retained"));
    let first_admission = harness
        .journal
        .begin_allocation_action("controller-1", &allocation.allocation_id, "DeleteInstance")
        .unwrap_or_else(|_| panic!("begin first delete"))
        .unwrap_or_else(|| panic!("first delete allocation exists"));
    assert!(
        harness
            .journal
            .finish_allocation_action(
                first_admission
                    .action_id
                    .unwrap_or_else(|| panic!("first delete action is admitted")),
                "DeleteInstance",
                "complete",
                None,
            )
            .unwrap_or_else(|_| panic!("complete first delete"))
    );
    wait_for_test_time(first_delete + 2).await;
    harness
        .journal
        .schedule_allocation_cleanup(&allocation.allocation_id, "retried planned delete")
        .unwrap_or_else(|_| panic!("persist retry delete intent"));
    let retry = harness
        .journal
        .begin_allocation_action("controller-1", &allocation.allocation_id, "DeleteInstance")
        .unwrap_or_else(|_| panic!("begin retry delete"))
        .unwrap_or_else(|| panic!("retry delete allocation exists"));
    assert!(retry.action_id.is_some());
    let event_time = chrono::DateTime::from_timestamp(
        i64::try_from(first_delete).unwrap_or(i64::MAX) + 1,
        900_000_000,
    )
    .unwrap_or_else(|| panic!("event after first delete but before retry"))
    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    *harness
        .spot_event_time
        .lock()
        .unwrap_or_else(|_| panic!("set Spot fixture source time")) = event_time;
    assert_eq!(
        harness
            .service
            .observe_interruptions(8)
            .await
            .unwrap_or_else(|_| panic!("observe after delete retry")),
        0
    );
    assert!(
        harness
            .journal
            .interruption_events(&allocation.allocation_id)
            .unwrap_or_else(|_| panic!("read first-delete exclusion"))
            .is_empty()
    );
}

/// Checks that interruption polling progresses independently of slow cleanup.
///
/// # Panics
/// Panics when the local allocation fixture fails an assertion or setup step.
pub async fn interruption_poll_progress_is_independent() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        child_mode: "multiple_slow_delete".to_owned(),
        provider_timeout_seconds: 1,
        spot: true,
        ..AllocationHarnessOptions::default()
    })
    .await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap_or_else(|_| panic!("create first Spot runner through pinned child"));
    let mut second_request = harness.request.clone();
    second_request
        .bootstrap
        .as_mut()
        .unwrap_or_else(|| panic!("second Spot bootstrap"))
        .name = "runner-second".to_owned();
    harness
        .service
        .handle(second_request)
        .await
        .unwrap_or_else(|_| panic!("create second Spot runner through pinned child"));
    let first = harness
        .journal
        .allocation("controller-1", "runner-old-image")
        .unwrap_or_else(|_| panic!("read first Spot allocation"))
        .unwrap_or_else(|| panic!("first Spot allocation exists"));
    let second = harness
        .journal
        .allocation("controller-1", "runner-second")
        .unwrap_or_else(|_| panic!("read second Spot allocation"))
        .unwrap_or_else(|| panic!("second Spot allocation exists"));
    wait_for_test_time(
        first
            .dispatched_at
            .unwrap_or_else(|| panic!("first Spot dispatch"))
            .max(
                second
                    .dispatched_at
                    .unwrap_or_else(|| panic!("second Spot dispatch")),
            )
            + 2,
    )
    .await;
    let event_time =
        chrono::DateTime::<chrono::Utc>::from(SystemTime::now()) - chrono::Duration::seconds(1);
    *harness
        .spot_event_time
        .lock()
        .unwrap_or_else(|_| panic!("set Spot fixture source time")) =
        event_time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    harness.spot_slow_first.store(true, Ordering::Relaxed);
    assert!(matches!(
        harness.service.observe_interruptions(8).await,
        Err(AllocationError::Ambiguous)
    ));
    assert!(
        harness
            .journal
            .interruption_events(&first.allocation_id)
            .unwrap_or_else(|_| panic!("read delayed first Spot result"))
            .is_empty()
    );
    assert_eq!(
        harness
            .journal
            .interruption_events(&second.allocation_id)
            .unwrap_or_else(|_| panic!("read independent second Spot result"))
            .len(),
        1
    );
}

/// Verify durable event provenance and restart retention through the production service.
///
/// # Panics
/// Panics when the service fixture or its persisted evidence differs from the contract.
/// Checks interruption observation provenance.
///
/// # Panics
/// Panics when the local allocation fixture fails an assertion or setup step.
pub async fn observation_provenance_case() {
    let mut harness = AllocationHarness::new(AllocationHarnessOptions {
        spot: true,
        ..AllocationHarnessOptions::default()
    })
    .await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap_or_else(|_| panic!("create Spot runner through pinned child"));
    let allocation = harness.allocation();
    let dispatched_at = allocation
        .dispatched_at
        .unwrap_or_else(|| panic!("retained provider dispatch time"));
    wait_for_test_time(dispatched_at + 2).await;
    let event_time = chrono::DateTime::from_timestamp(
        i64::try_from(dispatched_at).unwrap_or(i64::MAX) + 1,
        900_000_000,
    )
    .unwrap_or_else(|| panic!("synthetic source time"))
    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    *harness
        .spot_event_time
        .lock()
        .unwrap_or_else(|_| panic!("set Spot fixture source time")) = event_time.clone();
    assert_eq!(
        harness
            .service
            .observe_interruptions(8)
            .await
            .unwrap_or_else(|_| panic!("observe through production recovery service")),
        1
    );
    let events = harness
        .journal
        .interruption_events(&allocation.allocation_id)
        .unwrap_or_else(|_| panic!("read durable production observation"));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].source_api, "ec2:DescribeSpotInstanceRequests");
    assert_eq!(events[0].source_id, "sir-observed");
    let revision_time = events[0]
        .event_id
        .strip_prefix("sir-observed@")
        .unwrap_or_else(|| panic!("AWS request revision key includes source timestamp"));
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(revision_time)
            .unwrap_or_else(|_| panic!("parse AWS revision timestamp"))
            .timestamp_millis(),
        chrono::DateTime::parse_from_rfc3339(&event_time)
            .unwrap_or_else(|_| panic!("parse synthetic source time"))
            .timestamp_millis()
    );
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(&events[0].occurred_at)
            .unwrap_or_else(|_| panic!("parse persisted event time"))
            .timestamp_millis(),
        chrono::DateTime::parse_from_rfc3339(&event_time)
            .unwrap_or_else(|_| panic!("parse synthetic source time"))
            .timestamp_millis()
    );
    assert!(events[0].provider_pin_json.contains("aws-child"));
    let identity: outrider::cloud::PlacementIdentity =
        serde_json::from_str(&events[0].identity_json)
            .unwrap_or_else(|_| panic!("decode durable interruption identity"));
    let mut batch_extra = events[0].clone();
    batch_extra.event_id = "sir-batch-extra".to_owned();
    let mut conflict_first = events[0].clone();
    conflict_first.event_id = "sir-conflict".to_owned();
    let mut conflict_second = conflict_first.clone();
    conflict_second.reason_fields_json =
        r#"{"status_code":"instance-terminated-by-price"}"#.to_owned();
    assert!(
        harness
            .journal
            .record_interruptions(&[
                (&batch_extra, &identity),
                (&conflict_first, &identity),
                (&conflict_second, &identity),
            ])
            .is_err()
    );
    assert_eq!(
        harness
            .journal
            .interruption_events(&allocation.allocation_id)
            .unwrap_or_else(|_| panic!("read atomic-conflict result"))
            .len(),
        1
    );
    harness.service.0.take();
    harness.journal.0.take();
    let reopened = harness._fixture.journal();
    let persisted = reopened
        .interruption_events(&allocation.allocation_id)
        .unwrap_or_else(|_| panic!("reload production observation provenance"));
    assert_eq!(persisted, events);
}

/// Checks Spot observation handling for a selected event-time offset.
///
/// # Panics
/// Panics when the local allocation fixture fails an assertion or setup step.
pub async fn spot_observation_case(event_offset_from_dispatch: Option<i64>, expected: usize) {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        spot: true,
        ..AllocationHarnessOptions::default()
    })
    .await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap_or_else(|_| panic!("create Spot runner through pinned child"));
    let allocation = harness.allocation();
    let dispatched_at = allocation
        .dispatched_at
        .unwrap_or_else(|| panic!("retained provider dispatch time"));
    let event_time = match event_offset_from_dispatch {
        Some(0) => chrono::DateTime::from_timestamp(
            i64::try_from(dispatched_at).unwrap_or(i64::MAX),
            900_000_000,
        )
        .unwrap_or_else(|| panic!("same-second synthetic source time")),
        Some(900) => {
            chrono::DateTime::<chrono::Utc>::from(SystemTime::now())
                + chrono::Duration::milliseconds(900)
        }
        Some(offset) => chrono::DateTime::from_timestamp(
            i64::try_from(dispatched_at).unwrap_or(i64::MAX) + offset,
            900_000_000,
        )
        .unwrap_or_else(|| panic!("synthetic source time")),
        None => {
            wait_for_test_time(dispatched_at + 2).await;
            chrono::DateTime::from_timestamp(
                i64::try_from(dispatched_at).unwrap_or(i64::MAX) + 1,
                900_000_000,
            )
            .unwrap_or_else(|| panic!("synthetic source time"))
        }
    }
    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    *harness
        .spot_event_time
        .lock()
        .unwrap_or_else(|_| panic!("set Spot fixture source time")) = event_time;
    if event_offset_from_dispatch.is_none() {
        harness
            .journal
            .schedule_allocation_cleanup(&allocation.allocation_id, "cleanup after provider event")
            .unwrap_or_else(|_| panic!("persist cleanup after provider event"));
    }
    let result = harness.service.observe_interruptions(8).await;
    assert_eq!(
        result.unwrap_or_else(|_| panic!("observe Spot fixture")),
        expected
    );
    let events = harness
        .journal
        .interruption_events(&allocation.allocation_id)
        .unwrap_or_else(|_| panic!("read Spot observation"));
    assert_eq!(events.len(), expected);
    let action = harness
        .journal
        .allocation_action(&allocation.allocation_id)
        .unwrap_or_else(|_| panic!("read Spot cleanup intent"));
    assert_eq!(action.is_some(), expected == 1);
}

/// Checks that conflicting interruption sources do not create false evidence.
///
/// # Panics
/// Panics when the local allocation fixture fails an assertion or setup step.
pub async fn spot_conflicting_source_case() {
    let harness = AllocationHarness::new(AllocationHarnessOptions {
        spot: true,
        ..AllocationHarnessOptions::default()
    })
    .await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap_or_else(|_| panic!("create Spot runner through pinned child"));
    let allocation = harness.allocation();
    harness.spot_conflict.store(true, Ordering::Relaxed);
    assert!(matches!(
        harness.service.observe_interruptions(8).await,
        Err(AllocationError::Ambiguous)
    ));
    assert!(
        harness
            .journal
            .interruption_events(&allocation.allocation_id)
            .unwrap_or_else(|_| panic!("read conflicting-source result"))
            .is_empty()
    );
    assert!(
        harness
            .journal
            .allocation_action(&allocation.allocation_id)
            .unwrap_or_else(|_| panic!("read conflicting-source cleanup state"))
            .is_none()
    );
}

/// Waits for the local test clock to reach a target.
///
/// # Panics
/// Panics when the local allocation fixture fails an assertion or setup step.
pub async fn wait_for_test_time(target: u64) {
    while unix_now_for_test() < target {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub(crate) async fn restart_retained_positive_absence_case() {
    let mut harness = AllocationHarness::new(AllocationHarnessOptions::default()).await;
    harness
        .service
        .handle(harness.request.clone())
        .await
        .unwrap_or_else(|_| panic!("create positive runner"));
    let allocation = harness.allocation();
    assert!(allocation.returned_at.is_some());
    assert!(allocation.native_identity_json.is_some());
    harness.native_visible.store(false, Ordering::Relaxed);
    harness
        .journal
        .request_allocation_cleanup("controller-1", Some("pool-1"))
        .unwrap_or_else(|_| panic!("persist positive cleanup intent"));
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
    let reopened = Arc::new(harness._fixture.journal());
    let service = Box::pin(rebuild_allocation_service(
        &harness._fixture,
        reopened.clone(),
        &harness.aws_endpoint,
        &harness.old_config,
        &harness.current_config,
        "normal",
    ))
    .await;
    harness.journal.0 = Some(reopened.clone());
    harness.service.0 = Some(service);
    harness
        .journal
        .recover_startup_allocations()
        .unwrap_or_else(|_| panic!("recover positive cleanup intent"));
    assert!(matches!(
        harness.service.recover_pending_cleanup(1).await,
        Ok(1)
    ));
    let recovered = harness.allocation();
    assert_eq!(recovered.state, "deleted");
    assert!(recovered.returned_at.is_some());
    let deletes = harness
        .lifecycle_commands()
        .into_iter()
        .filter(|command| command == "DeleteInstance")
        .count();
    let creates = harness
        .lifecycle_commands()
        .into_iter()
        .filter(|command| command == "CreateInstance")
        .count();
    assert_eq!(deletes, 1);
    assert_eq!(creates, 1, "recovery does not replay CreateInstance");
    assert!(harness.native_inspections.load(Ordering::Relaxed) >= 2);
}
