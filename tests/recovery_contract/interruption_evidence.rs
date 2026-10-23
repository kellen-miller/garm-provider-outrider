use super::*;

#[test]
fn running_history_barriers_gaps_and_provider_pins_are_scoped() {
    let fixture = RecoveryFixture::new();
    let journal = fixture.journal();
    let (allocation, _) = journal
        .reserve_allocation("history-test", "controller", "runner", "snapshot", "pool")
        .unwrap_or_else(|_| panic!("reserve history test allocation"));
    let records = [
        ("same-time", "session-a", 1000, true),
        ("same-time", "session-a", 1000, false),
        ("same-time", "session-a", 2000, true),
        ("pinned", "session-b", 3000, true),
        ("pinned", "session-b", 4000, true),
        ("gap", "session-c", 5000, true),
        ("gap", "session-c", 16000, true),
    ];
    for (candidate_key, session_id, observed_at_millis, running) in records {
        journal
            .record_running_history(
                &RunningHistoryRecord {
                    allocation_id: allocation.allocation_id.clone(),
                    candidate_key: candidate_key.to_owned(),
                    catalog_fingerprint: "catalog".to_owned(),
                    provider_pin_json: "provider".to_owned(),
                    identity_json: "{}".to_owned(),
                    session_id: session_id.to_owned(),
                    observed_at_millis,
                    running,
                },
                0,
            )
            .unwrap_or_else(|_| panic!("persist history test sample"));
    }
    let barrier = journal
        .candidate_history("same-time", "catalog", "provider", 0, 3000, 2000)
        .unwrap_or_else(|_| panic!("read barrier history"));
    assert_eq!(barrier.running_exposure_millis, 0);
    assert_eq!(barrier.latest_observation_millis, None);

    let pinned = journal
        .candidate_history("pinned", "catalog", "provider", 0, 5000, 1000)
        .unwrap_or_else(|_| panic!("read pinned history"));
    assert_eq!(pinned.running_exposure_millis, 1000);
    let wrong_provider = journal
        .candidate_history("pinned", "catalog", "other-provider", 0, 5000, 1000)
        .unwrap_or_else(|_| panic!("read wrong-provider history"));
    assert_eq!(wrong_provider.running_exposure_millis, 0);

    let gap = journal
        .candidate_history("gap", "catalog", "provider", 0, 20000, 10000)
        .unwrap_or_else(|_| panic!("read gap history"));
    assert_eq!(gap.running_exposure_millis, 0);
    assert_eq!(gap.latest_observation_millis, None);
}
