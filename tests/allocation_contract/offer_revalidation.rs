use super::*;

#[tokio::test]
async fn quote_rejection_after_selection_releases_local_ownership() {
    let harness = AllocationHarness::new(AllocationHarnessOptions::default()).await;
    harness.quote_outage.store(true, Ordering::Relaxed);
    assert!(matches!(
        harness.service.handle(harness.request.clone()).await,
        Err(AllocationError::NoCandidate)
    ));
    let allocation = harness.allocation();
    assert_eq!(
        allocation.detail.as_deref(),
        Some("price quote unavailable")
    );
    assert_eq!(allocation.state, "no_candidate");
    assert!(allocation.dispatched_at.is_none());
    assert_eq!(
        harness
            .journal
            .active_allocation_counts("controller-1", "pool-1")
            .unwrap(),
        (0, 0)
    );
    assert!(harness.lifecycle_commands().is_empty());
}

#[tokio::test]
async fn ordered_allocation_rejects_launch_validation_failure_but_allows_price_outage() {
    let metadata_changed = AllocationHarness::new_with_quote_policy(
        AllocationHarnessOptions::default(),
        HarnessQuotePolicy {
            ordered: true,
            cost_ceiling: false,
            subnet_metadata_changes: true,
            max_price_age_seconds: None,
            economy_max_price_age_seconds: None,
        },
    )
    .await;
    let result = metadata_changed
        .service
        .handle(metadata_changed.request.clone())
        .await;
    assert!(
        matches!(result, Err(AllocationError::Catalog)),
        "{result:?}"
    );
    assert!(
        !metadata_changed
            .lifecycle_commands()
            .contains(&"CreateInstance".to_owned())
    );
    assert_eq!(
        metadata_changed
            .compute_quote_requests
            .load(Ordering::Relaxed),
        0,
        "changed subnet facts fail before any pricing request"
    );

    let price_outage = AllocationHarness::new_with_quote_policy(
        AllocationHarnessOptions::default(),
        HarnessQuotePolicy {
            ordered: true,
            cost_ceiling: false,
            subnet_metadata_changes: false,
            max_price_age_seconds: None,
            economy_max_price_age_seconds: None,
        },
    )
    .await;
    price_outage.quote_outage.store(true, Ordering::Relaxed);
    assert!(
        price_outage
            .service
            .handle(price_outage.request.clone())
            .await
            .is_ok()
    );
    assert!(
        price_outage
            .lifecycle_commands()
            .contains(&"CreateInstance".to_owned())
    );

    let price_outage_with_ceiling = AllocationHarness::new_with_quote_policy(
        AllocationHarnessOptions::default(),
        HarnessQuotePolicy {
            ordered: true,
            cost_ceiling: true,
            subnet_metadata_changes: false,
            max_price_age_seconds: None,
            economy_max_price_age_seconds: None,
        },
    )
    .await;
    price_outage_with_ceiling
        .quote_outage
        .store(true, Ordering::Relaxed);
    assert!(matches!(
        price_outage_with_ceiling
            .service
            .handle(price_outage_with_ceiling.request.clone())
            .await,
        Err(AllocationError::NoCandidate)
    ));
    assert!(
        !price_outage_with_ceiling
            .lifecycle_commands()
            .contains(&"CreateInstance".to_owned())
    );
}

#[tokio::test]
async fn late_offer_revalidation_ages_earlier_price_before_economy_ranking() {
    let harness = AllocationHarness::new_with_quote_policy(
        AllocationHarnessOptions::default(),
        HarnessQuotePolicy {
            ordered: false,
            cost_ceiling: false,
            subnet_metadata_changes: false,
            max_price_age_seconds: Some(600),
            economy_max_price_age_seconds: Some(0),
        },
    )
    .await;
    harness._image_delay_on_request.store(3, Ordering::Relaxed);
    harness
        ._image_request_delay_ms
        .store(1200, Ordering::Relaxed);

    let started = Instant::now();
    let result = harness.service.handle(harness.request.clone()).await;
    assert!(
        started.elapsed() >= Duration::from_millis(1050),
        "third DescribeImages response must delay later offer pricing by at least 1050ms"
    );

    assert!(
        matches!(result, Err(AllocationError::NoCandidate)),
        "strict economy freshness must reject the early quote after later SDK validation: {result:?}"
    );
    assert_eq!(harness._image_requests.load(Ordering::Relaxed), 4);
    assert_eq!(harness._delayed_image_requests.load(Ordering::Relaxed), 1);
    assert_eq!(
        harness.compute_quote_requests.load(Ordering::Relaxed),
        6,
        "two AWS offers each fetch compute, disk, and public-address prices"
    );
    let request_trace = harness
        ._request_trace
        .lock()
        .unwrap_or_else(|_| panic!("AWS request trace"))
        .clone();
    let delayed_image = request_trace
        .iter()
        .position(|request| request == "DescribeImages #3")
        .unwrap_or_else(|| panic!("third image revalidation in trace: {request_trace:?}"));
    let earlier_price = request_trace
        .iter()
        .position(|request| request.starts_with("GetProducts"))
        .unwrap_or_else(|| panic!("first price request in trace: {request_trace:?}"));
    let later_price = request_trace
        .iter()
        .rposition(|request| request.starts_with("GetProducts"))
        .unwrap_or_else(|| panic!("last price request in trace: {request_trace:?}"));
    assert!(
        earlier_price < delayed_image && delayed_image < later_price,
        "later image/launch revalidation delayed between actual quote requests: {request_trace:?}"
    );
    assert!(
        !harness
            .lifecycle_commands()
            .contains(&"CreateInstance".to_owned()),
        "stale quote rejection must happen before child Create"
    );
}
