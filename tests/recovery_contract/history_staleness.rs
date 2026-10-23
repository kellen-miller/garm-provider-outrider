use super::*;

#[tokio::test]
async fn restart_barrier_and_samples_do_not_refresh_history() {
    use outrider::{
        journal::PoolSnapshot,
        policy::{MissingDataBehavior, Policy, PolicyDefinition, WeightedPolicy},
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
    let policies = central
        .policies
        .as_mut()
        .unwrap_or_else(|| panic!("policy definitions"));
    policies.insert(
        "interruption-samples".to_owned(),
        PolicyDefinition {
            strategy: Policy::Weighted(WeightedPolicy {
                cost_weight: rust_decimal::Decimal::ZERO,
                failure_weight: rust_decimal::Decimal::ZERO,
                interruption_weight: rust_decimal::Decimal::ONE,
                latency_weight: rust_decimal::Decimal::ZERO,
                cost_scale: rust_decimal::Decimal::ONE,
                interruption_scale: rust_decimal::Decimal::ONE,
                latency_scale: rust_decimal::Decimal::ONE,
                min_samples: 2,
                max_observation_age_seconds: 600,
                max_price_age_seconds: 600,
            }),
            missing_data: MissingDataBehavior::Reject,
        },
    );
    policies.insert(
        "interruption-fresh".to_owned(),
        PolicyDefinition {
            strategy: Policy::Weighted(WeightedPolicy {
                cost_weight: rust_decimal::Decimal::ZERO,
                failure_weight: rust_decimal::Decimal::ZERO,
                interruption_weight: rust_decimal::Decimal::ONE,
                latency_weight: rust_decimal::Decimal::ZERO,
                cost_scale: rust_decimal::Decimal::ONE,
                interruption_scale: rust_decimal::Decimal::ONE,
                latency_scale: rust_decimal::Decimal::ONE,
                min_samples: 1,
                max_observation_age_seconds: 1,
                max_price_age_seconds: 600,
            }),
            missing_data: MissingDataBehavior::Reject,
        },
    );
    for (snapshot_id, token, policy) in [
        (
            "snapshot-history-samples",
            "history-samples-token",
            "interruption-samples",
        ),
        (
            "snapshot-history-stale",
            "history-stale-token",
            "interruption-fresh",
        ),
    ] {
        let mut document = allocation_harness::snapshot_document(central.clone(), token);
        document.resolved_profile.policy = policy.to_owned();
        document.resolved_profile.purchasing_models =
            std::collections::BTreeSet::from(["spot".to_owned()]);
        harness
            .journal
            .save_pool_snapshot(&PoolSnapshot {
                id: snapshot_id.to_owned(),
                repository_id: 7,
                request_token: token.to_owned(),
                config_json: serde_json::to_string(&document)
                    .unwrap_or_else(|_| panic!("serialize history policy snapshot")),
                state: "enabled".to_owned(),
                garm_pool_id: Some("pool-1".to_owned()),
                expires_at: unix_now() + 3600,
            })
            .unwrap_or_else(|_| panic!("persist history policy snapshot"));
    }
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

    let mut insufficient = harness.request.clone();
    set_history_request(
        &mut insufficient,
        "runner-sample-rejected",
        "history-samples-token",
        "snapshot-history-samples",
    );
    assert!(matches!(
        harness.service.handle(insufficient).await,
        Err(outrider::allocation::AllocationError::NoCandidate)
    ));

    harness
        .spot_conflict
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(matches!(
        harness.service.observe_interruptions(8).await,
        Err(outrider::allocation::AllocationError::Ambiguous)
    ));
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    harness
        .spot_conflict
        .store(false, std::sync::atomic::Ordering::Relaxed);
    harness
        .native_inspection_mode
        .store(1, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        harness
            .service
            .observe_interruptions(8)
            .await
            .unwrap_or_else(|_| panic!("record post-barrier running point")),
        0
    );

    let mut stale = harness.request.clone();
    set_history_request(
        &mut stale,
        "runner-stale-rejected",
        "history-stale-token",
        "snapshot-history-stale",
    );
    assert!(matches!(
        harness.service.handle(stale).await,
        Err(outrider::allocation::AllocationError::NoCandidate)
    ));
}
