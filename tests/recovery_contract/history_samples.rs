use super::retention_history::seed_legacy_terminal_retention_rows;
use super::*;

#[test]
fn schema_fourteen_retention_migration_backfills_in_bounded_batches() {
    let fixture = RecoveryFixture::new();
    let journal = fixture.journal();
    seed_legacy_terminal_retention_rows(&journal);
    let pending_delivery = WebhookDelivery {
        delivery_id: "legacy-pending-delivery".to_owned(),
        repository_id: 41,
        event: "workflow_job".to_owned(),
        headers_json: "{\"pending\":true}".to_owned(),
        body: b"legacy-pending-body".to_vec(),
        state: "received".to_owned(),
    };
    assert!(journal.persist_delivery(&pending_delivery).unwrap());
    let pending_outbox = OutboxMessage {
        id: "legacy-pending-outbox".to_owned(),
        kind: "forward".to_owned(),
        destination: "https://github.invalid".to_owned(),
        headers_json: "{\"pending\":true}".to_owned(),
        body: b"legacy-pending-outbox-body".to_vec(),
        state: "pending".to_owned(),
    };
    journal.enqueue_outbox(&pending_outbox).unwrap();
    let pending_snapshot = PoolSnapshot {
        id: "legacy-pending-snapshot".to_owned(),
        repository_id: 41,
        request_token: "legacy-pending-token".to_owned(),
        config_json: r#"{"pending_secret":"keep"}"#.to_owned(),
        state: "creating".to_owned(),
        garm_pool_id: None,
        expires_at: i64::MAX as u64,
    };
    journal.save_pool_snapshot(&pending_snapshot).unwrap();
    drop(journal);

    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    connection
        .execute(
            "UPDATE webhook_deliveries SET created_at=1 WHERE delivery_id LIKE 'legacy-terminal-%'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE ingress_outbox SET created_at=1 WHERE id LIKE 'legacy-outbox-%'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE pool_snapshots SET created_at=1 WHERE id LIKE 'legacy-snapshot-%'",
            [],
        )
        .unwrap();
    connection
        .execute_batch(
            "DROP TABLE history_retention_floors;
             ALTER TABLE webhook_deliveries DROP COLUMN terminal_at;
             ALTER TABLE webhook_deliveries DROP COLUMN payload_sha256;
             ALTER TABLE ingress_outbox DROP COLUMN terminal_at;
             ALTER TABLE ingress_outbox DROP COLUMN payload_sha256;
             ALTER TABLE pool_snapshots DROP COLUMN terminal_at;
             ALTER TABLE pool_snapshots DROP COLUMN compacted_at;
             ALTER TABLE pool_snapshots DROP COLUMN config_sha256;
             UPDATE schema_version SET version=14;",
        )
        .unwrap();
    drop(connection);

    let migration_started = unix_now();
    let journal = fixture.journal();
    let migration_finished = unix_now();
    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    let schema_version: u32 = connection
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(schema_version, 16);
    let retention_backfill_counts: (i64, i64, i64, i64, i64, i64, i64, i64, i64) = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM webhook_deliveries WHERE delivery_id LIKE 'legacy-terminal-%' AND terminal_at IS NOT NULL), (SELECT COUNT(*) FROM webhook_deliveries WHERE delivery_id LIKE 'legacy-terminal-%' AND payload_sha256 IS NOT NULL), (SELECT COUNT(*) FROM ingress_outbox WHERE id LIKE 'legacy-outbox-%' AND terminal_at IS NOT NULL), (SELECT COUNT(*) FROM ingress_outbox WHERE id LIKE 'legacy-outbox-%' AND payload_sha256 IS NOT NULL), (SELECT COUNT(*) FROM pool_snapshots WHERE id LIKE 'legacy-snapshot-%' AND terminal_at IS NOT NULL), (SELECT COUNT(*) FROM pool_snapshots WHERE id LIKE 'legacy-snapshot-%' AND config_sha256 IS NOT NULL), (SELECT COUNT(*) FROM webhook_deliveries WHERE delivery_id='legacy-pending-delivery' AND terminal_at IS NULL), (SELECT COUNT(*) FROM ingress_outbox WHERE id='legacy-pending-outbox' AND terminal_at IS NULL), (SELECT COUNT(*) FROM pool_snapshots WHERE id='legacy-pending-snapshot' AND terminal_at IS NULL)",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .unwrap_or_else(|_| panic!("count schema15 retention backfill rows"));
    assert_eq!(retention_backfill_counts, (35, 35, 35, 35, 35, 35, 1, 1, 1));
    let terminal_times: (i64, i64, i64) = connection
        .query_row(
            "SELECT (SELECT MIN(terminal_at) FROM webhook_deliveries WHERE delivery_id LIKE 'legacy-terminal-%'), (SELECT MIN(terminal_at) FROM ingress_outbox WHERE id LIKE 'legacy-outbox-%'), (SELECT MIN(terminal_at) FROM pool_snapshots WHERE id LIKE 'legacy-snapshot-%')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let terminal_maxima: (i64, i64, i64) = connection
        .query_row(
            "SELECT (SELECT MAX(terminal_at) FROM webhook_deliveries WHERE delivery_id LIKE 'legacy-terminal-%'), (SELECT MAX(terminal_at) FROM ingress_outbox WHERE id LIKE 'legacy-outbox-%'), (SELECT MAX(terminal_at) FROM pool_snapshots WHERE id LIKE 'legacy-snapshot-%')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let minimum = i64::try_from(migration_started).unwrap().saturating_sub(1);
    let maximum = i64::try_from(migration_finished).unwrap().saturating_add(1);
    assert!(terminal_times.0 >= minimum && terminal_maxima.0 <= maximum);
    assert!(terminal_times.1 >= minimum && terminal_maxima.1 <= maximum);
    assert!(terminal_times.2 >= minimum && terminal_maxima.2 <= maximum);
    assert_eq!(
        journal
            .pending_deliveries(10)
            .unwrap()
            .into_iter()
            .map(|delivery| delivery.delivery_id)
            .collect::<Vec<_>>(),
        vec![pending_delivery.delivery_id.clone()]
    );
    assert_eq!(
        journal.pending_outbox(10).unwrap(),
        vec![pending_outbox.clone()]
    );
    let pending_snapshot_after_migration = journal
        .pool_snapshot_by_id(&pending_snapshot.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        pending_snapshot_after_migration.config_json,
        pending_snapshot.config_json
    );
    assert_eq!(pending_snapshot_after_migration.state, "creating");
    drop(connection);

    let first_delivery = WebhookDelivery {
        delivery_id: "legacy-terminal-0".to_owned(),
        repository_id: 41,
        event: "workflow_job".to_owned(),
        headers_json: r#"{"x-legacy":"0"}"#.to_owned(),
        body: b"legacy-terminal-payload-0".to_vec(),
        state: "received".to_owned(),
    };
    assert!(!journal.persist_delivery(&first_delivery).unwrap());
    let mut changed_headers = first_delivery.clone();
    changed_headers.headers_json = r#"{"x-legacy":"changed"}"#.to_owned();
    assert!(journal.persist_delivery(&changed_headers).is_err());
    let mut changed_repository = first_delivery.clone();
    changed_repository.repository_id = 42;
    assert!(journal.persist_delivery(&changed_repository).is_err());
    let first_outbox = OutboxMessage {
        id: "legacy-outbox-0".to_owned(),
        kind: "check".to_owned(),
        destination: "https://github.invalid".to_owned(),
        headers_json: r#"{"x-legacy":"0"}"#.to_owned(),
        body: b"legacy-outbox-payload-0".to_vec(),
        state: "pending".to_owned(),
    };
    journal.enqueue_outbox(&first_outbox).unwrap();
    let mut changed_outbox_headers = first_outbox.clone();
    changed_outbox_headers.headers_json = r#"{"x-legacy":"changed"}"#.to_owned();
    assert!(journal.enqueue_outbox(&changed_outbox_headers).is_err());
    assert_eq!(journal.maintain_retention(0, 32).unwrap(), 0);
    drop(journal);

    let reopened = fixture.journal();
    assert!(!reopened.persist_delivery(&first_delivery).unwrap());
    assert_eq!(
        reopened.pending_deliveries(10).unwrap()[0].body,
        pending_delivery.body
    );
    assert_eq!(reopened.pending_outbox(10).unwrap(), vec![pending_outbox]);
    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    let retained_payload_counts: (i64, i64, i64) = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM webhook_deliveries WHERE delivery_id LIKE 'legacy-terminal-%' AND length(body)>0), (SELECT COUNT(*) FROM ingress_outbox WHERE id LIKE 'legacy-outbox-%' AND length(body)>0), (SELECT COUNT(*) FROM pool_snapshots WHERE id LIKE 'legacy-snapshot-%' AND config_json LIKE '%legacy_secret%')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(retained_payload_counts, (35, 35, 35));
}

#[test]
fn retention_prunes_old_running_samples_and_marks_expanded_windows_unknown() {
    let fixture = RecoveryFixture::new();
    let journal = fixture.journal();
    let (allocation, _) = journal
        .reserve_allocation(
            "retention-history",
            "controller",
            "runner",
            "snapshot",
            "pool",
        )
        .unwrap();
    for observed_at_millis in [1_000, 2_000] {
        journal
            .record_running_history(
                &RunningHistoryRecord {
                    allocation_id: allocation.allocation_id.clone(),
                    candidate_key: "retention-candidate".to_owned(),
                    catalog_fingerprint: "retention-catalog".to_owned(),
                    provider_pin_json: "retention-provider".to_owned(),
                    identity_json: "{}".to_owned(),
                    session_id: "retention-session".to_owned(),
                    observed_at_millis,
                    running: true,
                },
                0,
            )
            .unwrap();
    }
    assert_eq!(journal.maintain_retention(0, 32).unwrap(), 2);
    let expanded = journal
        .candidate_history(
            "retention-candidate",
            "retention-catalog",
            "retention-provider",
            0,
            i64::MAX as u128,
            60_000,
        )
        .unwrap();
    assert!(!expanded.running_coverage_complete);
    assert_eq!(expanded.running_exposure_millis, 0);
    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    let floor_millis: i64 = connection
        .query_row(
            "SELECT floor_millis FROM history_retention_floors WHERE candidate_key='retention-candidate' AND dimension='running'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);
    let at_removed_sample = journal
        .candidate_history(
            "retention-candidate",
            "retention-catalog",
            "retention-provider",
            u128::try_from(floor_millis).unwrap(),
            i64::MAX as u128,
            60_000,
        )
        .unwrap();
    assert!(!at_removed_sample.running_coverage_complete);
    let after_removed_sample = journal
        .candidate_history(
            "retention-candidate",
            "retention-catalog",
            "retention-provider",
            u128::try_from(floor_millis + 1).unwrap(),
            i64::MAX as u128,
            60_000,
        )
        .unwrap();
    assert!(after_removed_sample.running_coverage_complete);
}

pub(super) struct RetentionSample {
    pub(super) allocation_id: &'static str,
    pub(super) age_millis: i64,
    pub(super) latency_nanos: u64,
}

pub(super) fn record_retention_sample(
    journal: &Journal,
    sample: &RetentionSample,
    now_millis: i64,
) {
    let candidate_key = "retention-metrics-candidate";
    let catalog_fingerprint = "retention-metrics-catalog";
    let provider_pin_json = r#"{"child":"sha256:retained-pin"}"#;
    persist_dispatched_history_attempt(
        journal,
        sample.allocation_id,
        sample.allocation_id,
        r#"{"offer":"retained"}"#,
        r#"{"provider_pin":"sha256:retained-pin"}"#,
        "retention-account",
        catalog_fingerprint,
    );
    let observed_at_millis = now_millis.saturating_sub(sample.age_millis);
    journal
        .finish_child_create(
            1,
            &CreateHistoryRecord {
                allocation_id: sample.allocation_id.to_owned(),
                candidate_key: candidate_key.to_owned(),
                catalog_fingerprint: catalog_fingerprint.to_owned(),
                provider_pin_json: provider_pin_json.to_owned(),
                observed_at_millis: u128::try_from(observed_at_millis)
                    .unwrap_or_else(|_| panic!("positive retention timestamp")),
                latency_nanos: Some(sample.latency_nanos),
            },
        )
        .unwrap_or_else(|_| panic!("persist retention create history"));
    let identity_json = format!(
        r#"{{"controller_id":"controller-1","cloud":"aws","account_id":"retention-account","location":"us-east-1","resource_id":"native-{}","host_id":null,"dependency_ids":{{}}}}"#,
        sample.allocation_id
    );
    assert!(
        journal
            .complete_allocation(sample.allocation_id, "{\"instance\":true}", &identity_json)
            .unwrap_or_else(|_| panic!("retain positive native create evidence"))
    );
    for observed_at_millis in [observed_at_millis, observed_at_millis + 3_600_000] {
        journal
            .record_running_history(
                &RunningHistoryRecord {
                    allocation_id: sample.allocation_id.to_owned(),
                    candidate_key: candidate_key.to_owned(),
                    catalog_fingerprint: catalog_fingerprint.to_owned(),
                    provider_pin_json: provider_pin_json.to_owned(),
                    identity_json: identity_json.clone(),
                    session_id: sample.allocation_id.to_owned(),
                    observed_at_millis: u128::try_from(observed_at_millis)
                        .unwrap_or_else(|_| panic!("positive retention sample timestamp")),
                    running: true,
                },
                0,
            )
            .unwrap_or_else(|_| panic!("persist retention running sample"));
    }
}

pub(super) fn assert_configured_retention_window(
    journal: &Journal,
    now_millis: i64,
    day_millis: i64,
) {
    let configured_window = journal
        .candidate_history(
            "retention-metrics-candidate",
            "retention-metrics-catalog",
            r#"{"child":"sha256:retained-pin"}"#,
            u128::try_from(now_millis - 120 * day_millis)
                .unwrap_or_else(|_| panic!("positive configured history start")),
            u128::try_from(now_millis)
                .unwrap_or_else(|_| panic!("positive configured history end")),
            4 * 60 * 60 * 1000,
        )
        .unwrap_or_else(|_| panic!("read configured retention history"));
    assert!(configured_window.latency_coverage_complete);
    assert!(configured_window.running_coverage_complete);
    assert_eq!(configured_window.definitive_create_outcomes, 2);
    assert_eq!(
        configured_window.create_latencies_nanos,
        vec![100_000, 2_000]
    );
    assert_eq!(configured_window.running_exposure_millis, 2 * 3_600_000);
}

pub(super) fn assert_expanded_and_recent_retention_windows(
    journal: &Journal,
    now_millis: i64,
    day_millis: i64,
) {
    let expanded_window = journal
        .candidate_history(
            "retention-metrics-candidate",
            "retention-metrics-catalog",
            r#"{"child":"sha256:retained-pin"}"#,
            u128::try_from(now_millis - 120 * day_millis)
                .unwrap_or_else(|_| panic!("positive expanded history start")),
            u128::try_from(now_millis).unwrap_or_else(|_| panic!("positive expanded history end")),
            4 * 60 * 60 * 1000,
        )
        .unwrap_or_else(|_| panic!("read expanded retention history"));
    assert!(!expanded_window.latency_coverage_complete);
    assert!(!expanded_window.running_coverage_complete);
    assert_eq!(expanded_window.create_latencies_nanos, Vec::<u64>::new());
    assert_eq!(expanded_window.running_exposure_millis, 0);
    assert_eq!(expanded_window.definitive_create_outcomes, 2);

    let recent_window = journal
        .candidate_history(
            "retention-metrics-candidate",
            "retention-metrics-catalog",
            r#"{"child":"sha256:retained-pin"}"#,
            u128::try_from(now_millis - 24 * 60 * 60 * 1000)
                .unwrap_or_else(|_| panic!("positive recent history start")),
            u128::try_from(now_millis).unwrap_or_else(|_| panic!("positive recent history end")),
            4 * 60 * 60 * 1000,
        )
        .unwrap_or_else(|_| panic!("read recent retention history"));
    assert!(recent_window.latency_coverage_complete);
    assert!(recent_window.running_coverage_complete);
    assert_eq!(recent_window.create_latencies_nanos, vec![2_000]);
    assert_eq!(recent_window.running_exposure_millis, 3_600_000);
}

pub(super) fn assert_retained_create_evidence(
    fixture: &RecoveryFixture,
    journal: &Journal,
    samples: &[RetentionSample],
    now_millis: i64,
    day_millis: i64,
) {
    let connection = rusqlite::Connection::open(&fixture.database)
        .unwrap_or_else(|_| panic!("open retained create evidence"));
    let retained_create: (i64, Option<i64>) = connection
        .query_row(
            "SELECT observed_at_millis, latency_nanos FROM create_history WHERE allocation_id='retention-create-100d'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or_else(|_| panic!("read retained create timestamp and latency"));
    assert_eq!(retained_create.0, now_millis - 100 * day_millis);
    assert_eq!(retained_create.1, None);
    let running_floor: i64 = connection
        .query_row(
            "SELECT floor_millis FROM history_retention_floors WHERE candidate_key='retention-metrics-candidate' AND catalog_fingerprint='retention-metrics-catalog' AND provider_pin_json=?1 AND dimension='running'",
            [r#"{"child":"sha256:retained-pin"}"#],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| panic!("read running retention floor"));
    let latency_floor: i64 = connection
        .query_row(
            "SELECT floor_millis FROM history_retention_floors WHERE candidate_key='retention-metrics-candidate' AND catalog_fingerprint='retention-metrics-catalog' AND provider_pin_json=?1 AND dimension='latency'",
            [r#"{"child":"sha256:retained-pin"}"#],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| panic!("read latency retention floor"));
    assert_eq!(running_floor, now_millis - 100 * day_millis + 3_600_000);
    assert_eq!(latency_floor, now_millis - 100 * day_millis);
    for sample in samples {
        let allocation = journal
            .allocation("controller-1", sample.allocation_id)
            .unwrap_or_else(|_| panic!("read retained allocation"))
            .unwrap_or_else(|| panic!("retained allocation exists"));
        assert_eq!(allocation.state, "created");
        assert_eq!(
            allocation.binding_json.as_deref(),
            Some(r#"{"provider_pin":"sha256:retained-pin"}"#)
        );
        assert!(
            allocation.native_identity_json.as_deref().is_some_and(
                |identity| identity.contains(&format!("native-{}", sample.allocation_id))
            )
        );
    }
    drop(connection);
}

#[test]
fn retention_preserves_configured_window_create_evidence_and_recent_metrics() {
    let fixture = RecoveryFixture::new();
    let journal = fixture.journal();
    journal
        .save_pool_snapshot(&PoolSnapshot {
            id: "snapshot-history".to_owned(),
            repository_id: 7,
            request_token: "retention-metrics".to_owned(),
            config_json: "{}".to_owned(),
            state: "enabled".to_owned(),
            garm_pool_id: Some("pool-1".to_owned()),
            expires_at: unix_now().saturating_add(3600),
        })
        .unwrap_or_else(|_| panic!("save retention history snapshot"));
    let now_millis =
        i64::try_from(unix_now()).unwrap_or_else(|_| panic!("positive retention wall time")) * 1000;
    let day_millis = 24 * 60 * 60 * 1000;
    let samples = [
        RetentionSample {
            allocation_id: "retention-create-130d",
            age_millis: 130 * day_millis,
            latency_nanos: 130_000,
        },
        RetentionSample {
            allocation_id: "retention-create-100d",
            age_millis: 100 * day_millis,
            latency_nanos: 100_000,
        },
        RetentionSample {
            allocation_id: "retention-create-recent",
            age_millis: 2 * 60 * 60 * 1000,
            latency_nanos: 2_000,
        },
    ];
    for sample in &samples {
        record_retention_sample(&journal, sample, now_millis);
    }

    assert!(
        journal
            .maintain_retention(120 * 24 * 60 * 60, 32)
            .unwrap_or_else(|_| panic!("apply configured retention window"))
            > 0
    );
    assert_configured_retention_window(&journal, now_millis, day_millis);
    assert!(
        journal
            .maintain_retention(0, 32)
            .unwrap_or_else(|_| panic!("apply minimum retention window"))
            > 0
    );
    assert_expanded_and_recent_retention_windows(&journal, now_millis, day_millis);
    assert_retained_create_evidence(&fixture, &journal, &samples, now_millis, day_millis);
    drop(journal);

    let reopened = fixture.journal();
    let expanded_after_reopen = reopened
        .candidate_history(
            "retention-metrics-candidate",
            "retention-metrics-catalog",
            r#"{"child":"sha256:retained-pin"}"#,
            u128::try_from(now_millis - 120 * day_millis)
                .unwrap_or_else(|_| panic!("positive expanded history start")),
            u128::try_from(now_millis).unwrap_or_else(|_| panic!("positive expanded history end")),
            4 * 60 * 60 * 1000,
        )
        .unwrap_or_else(|_| panic!("read reopened expanded history"));
    assert!(!expanded_after_reopen.latency_coverage_complete);
    assert!(!expanded_after_reopen.running_coverage_complete);
}
