use super::*;

#[test]
fn retention_compacts_old_terminal_payloads_and_preserves_replay_identity() {
    let fixture = RecoveryFixture::new();
    let journal = fixture.journal();
    let terminal_delivery = WebhookDelivery {
        delivery_id: "retention-terminal-delivery".to_owned(),
        repository_id: 41,
        event: "workflow_job".to_owned(),
        headers_json: r#"{"x-hub-signature-256":"signature"}"#.to_owned(),
        body: br#"{"workflow_job":{"id":7}}"#.to_vec(),
        state: "received".to_owned(),
    };
    assert!(
        journal
            .persist_delivery_with_terminal(&terminal_delivery, Some((7, 1, true)))
            .unwrap()
    );
    journal
        .set_delivery_state(&terminal_delivery.delivery_id, "queued", None)
        .unwrap();
    let pending_delivery = WebhookDelivery {
        delivery_id: "retention-pending-delivery".to_owned(),
        repository_id: 41,
        event: "workflow_job".to_owned(),
        headers_json: "{}".to_owned(),
        body: b"pending-original".to_vec(),
        state: "received".to_owned(),
    };
    assert!(journal.persist_delivery(&pending_delivery).unwrap());
    let retry_delivery = WebhookDelivery {
        delivery_id: "retention-retry-delivery".to_owned(),
        repository_id: 41,
        event: "workflow_job".to_owned(),
        headers_json: "{}".to_owned(),
        body: b"retry-original".to_vec(),
        state: "retry".to_owned(),
    };
    assert!(journal.persist_delivery(&retry_delivery).unwrap());
    journal
        .set_delivery_state(&retry_delivery.delivery_id, "retry", Some("temporary"))
        .unwrap();
    let terminal_outbox = OutboxMessage {
        id: "retention-terminal-outbox".to_owned(),
        kind: "check".to_owned(),
        destination: "https://github.invalid".to_owned(),
        headers_json: r#"{"authorization":"secret"}"#.to_owned(),
        body: b"terminal-outbox-body".to_vec(),
        state: "pending".to_owned(),
    };
    journal.enqueue_outbox(&terminal_outbox).unwrap();
    journal
        .set_outbox_state(&terminal_outbox.id, "delivered", None)
        .unwrap();
    let pending_outbox = OutboxMessage {
        id: "retention-pending-outbox".to_owned(),
        kind: "check".to_owned(),
        destination: "https://github.invalid".to_owned(),
        headers_json: "{}".to_owned(),
        body: b"pending-outbox-original".to_vec(),
        state: "pending".to_owned(),
    };
    journal.enqueue_outbox(&pending_outbox).unwrap();
    let retry_outbox = OutboxMessage {
        id: "retention-retry-outbox".to_owned(),
        kind: "check".to_owned(),
        destination: "https://github.invalid".to_owned(),
        headers_json: "{}".to_owned(),
        body: b"retry-outbox-original".to_vec(),
        state: "retry".to_owned(),
    };
    journal.enqueue_outbox(&retry_outbox).unwrap();
    journal
        .set_outbox_state(&retry_outbox.id, "retry", Some("temporary"))
        .unwrap();
    let snapshot = PoolSnapshot {
        id: "retention-snapshot".to_owned(),
        repository_id: 41,
        request_token: "retention-token".to_owned(),
        config_json: r#"{"token":"retained-secret"}"#.to_owned(),
        state: "creating".to_owned(),
        garm_pool_id: Some("pool-retention".to_owned()),
        expires_at: i64::MAX as u64,
    };
    journal.save_pool_snapshot(&snapshot).unwrap();
    journal
        .set_pool_snapshot_state(&snapshot.id, "deleted", None)
        .unwrap();
    let guarded_snapshot = PoolSnapshot {
        id: "retention-live-snapshot".to_owned(),
        repository_id: 41,
        request_token: "retention-live-token".to_owned(),
        config_json: r#"{"retained":"for-active-allocation"}"#.to_owned(),
        state: "creating".to_owned(),
        garm_pool_id: Some("pool-live-retention".to_owned()),
        expires_at: i64::MAX as u64,
    };
    journal.save_pool_snapshot(&guarded_snapshot).unwrap();
    journal
        .set_pool_snapshot_state(&guarded_snapshot.id, "deleted", None)
        .unwrap();
    let (guarded_allocation, _) = journal
        .reserve_allocation(
            "retention-live-allocation",
            "controller",
            "runner-live-retention",
            &guarded_snapshot.id,
            "pool-live-retention",
        )
        .unwrap();
    drop(journal);

    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    connection
        .execute(
            "UPDATE webhook_deliveries SET terminal_at=1 WHERE delivery_id=?1",
            [&terminal_delivery.delivery_id],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE ingress_outbox SET terminal_at=1 WHERE id=?1",
            [&terminal_outbox.id],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE pool_snapshots SET terminal_at=1 WHERE id=?1",
            [&snapshot.id],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE pool_snapshots SET terminal_at=1 WHERE id=?1",
            [&guarded_snapshot.id],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE allocations SET state='ambiguous', dispatched_at=2 WHERE allocation_id=?1",
            [&guarded_allocation.allocation_id],
        )
        .unwrap();
    drop(connection);

    let journal = fixture.journal();
    assert!(journal.maintain_retention(0, 32).unwrap() >= 3);
    drop(journal);
    let journal = fixture.journal();
    assert!(!journal.persist_delivery(&terminal_delivery).unwrap());
    journal.enqueue_outbox(&terminal_outbox).unwrap();
    let mut altered_outbox = terminal_outbox.clone();
    altered_outbox.body.push(b'!');
    assert!(journal.enqueue_outbox(&altered_outbox).is_err());
    let mut altered_delivery = terminal_delivery.clone();
    altered_delivery.body.push(b'!');
    assert!(journal.persist_delivery(&altered_delivery).is_err());
    assert_eq!(
        journal.pending_deliveries(10).unwrap(),
        vec![pending_delivery, retry_delivery]
    );
    assert_eq!(
        journal.pending_outbox(10).unwrap(),
        vec![pending_outbox, retry_outbox]
    );
    let compacted = journal.pool_snapshot_by_id(&snapshot.id).unwrap().unwrap();
    assert!(compacted.config_json.contains("compacted"));
    let retained = journal
        .pool_snapshot_by_id(&guarded_snapshot.id)
        .unwrap()
        .unwrap();
    assert_eq!(retained.config_json, guarded_snapshot.config_json);
    journal
        .set_delivery_state(&terminal_delivery.delivery_id, "retry", Some("late replay"))
        .unwrap();
    journal
        .set_outbox_state(&terminal_outbox.id, "retry", Some("late replay"))
        .unwrap();
    assert_eq!(
        journal
            .delivery_state(&terminal_delivery.delivery_id)
            .unwrap()
            .as_deref(),
        Some("queued")
    );
    assert_eq!(
        journal
            .outbox_state(&terminal_outbox.id)
            .unwrap()
            .as_deref(),
        Some("delivered")
    );
    assert_eq!(
        journal
            .active_allocation_counts("controller", "pool-live-retention")
            .unwrap(),
        (1, 1)
    );
    assert!(journal.run_is_cancelled(41, 7, 1).unwrap());
    assert!(journal.save_pool_snapshot(&snapshot).is_err());
    let (same_name, created) = journal
        .reserve_allocation(
            "new-retention-allocation",
            "controller",
            "runner-live-retention",
            &guarded_snapshot.id,
            "pool-live-retention",
        )
        .unwrap();
    assert!(!created);
    assert_eq!(same_name.allocation_id, guarded_allocation.allocation_id);
    drop(journal);

    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    let terminal_payloads: (String, Vec<u8>, String, Vec<u8>) = connection
        .query_row(
            "SELECT delivery.headers_json, delivery.body, outbox.headers_json, outbox.body FROM webhook_deliveries delivery CROSS JOIN ingress_outbox outbox WHERE delivery.delivery_id=?1 AND outbox.id=?2",
            rusqlite::params![terminal_delivery.delivery_id, terminal_outbox.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        terminal_payloads,
        (String::new(), Vec::new(), String::new(), Vec::new())
    );
}

#[test]
fn recent_terminal_retention_uses_transition_time_not_created_at() {
    let fixture = RecoveryFixture::new();
    let journal = fixture.journal();
    let delivery = WebhookDelivery {
        delivery_id: "recent-terminal-delivery".to_owned(),
        repository_id: 41,
        event: "workflow_job".to_owned(),
        headers_json: "{\"signed\":true}".to_owned(),
        body: b"recent signed bytes".to_vec(),
        state: "received".to_owned(),
    };
    assert!(journal.persist_delivery(&delivery).unwrap());
    journal
        .set_delivery_state(&delivery.delivery_id, "queued", None)
        .unwrap();
    let outbox = OutboxMessage {
        id: "recent-terminal-outbox".to_owned(),
        kind: "check".to_owned(),
        destination: "https://github.invalid".to_owned(),
        headers_json: "{\"authorization\":\"secret\"}".to_owned(),
        body: b"recent check bytes".to_vec(),
        state: "pending".to_owned(),
    };
    journal.enqueue_outbox(&outbox).unwrap();
    journal
        .set_outbox_state(&outbox.id, "delivered", None)
        .unwrap();
    let snapshot = PoolSnapshot {
        id: "recent-terminal-snapshot".to_owned(),
        repository_id: 41,
        request_token: "recent-terminal-token".to_owned(),
        config_json: r#"{"secret":"recent config"}"#.to_owned(),
        state: "creating".to_owned(),
        garm_pool_id: Some("pool-recent-terminal".to_owned()),
        expires_at: i64::MAX as u64,
    };
    journal.save_pool_snapshot(&snapshot).unwrap();
    journal
        .set_pool_snapshot_state(&snapshot.id, "deleted", None)
        .unwrap();
    drop(journal);

    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    connection
        .execute(
            "UPDATE webhook_deliveries SET created_at=1 WHERE delivery_id=?1",
            [&delivery.delivery_id],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE ingress_outbox SET created_at=1 WHERE id=?1",
            [&outbox.id],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE pool_snapshots SET created_at=1 WHERE id=?1",
            [&snapshot.id],
        )
        .unwrap();
    drop(connection);

    let journal = fixture.journal();
    assert_eq!(journal.maintain_retention(0, 32).unwrap(), 0);
    assert_eq!(journal.pending_deliveries(10).unwrap().len(), 0);
    assert_eq!(journal.pending_outbox(10).unwrap().len(), 0);
    let retained_snapshot = journal.pool_snapshot_by_id(&snapshot.id).unwrap().unwrap();
    assert_eq!(retained_snapshot.config_json, snapshot.config_json);
    drop(journal);
    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    let retained_payloads: (Vec<u8>, Vec<u8>) = connection
        .query_row(
            "SELECT delivery.body, outbox.body FROM webhook_deliveries delivery CROSS JOIN ingress_outbox outbox WHERE delivery.delivery_id=?1 AND outbox.id=?2",
            rusqlite::params![delivery.delivery_id, outbox.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(retained_payloads, (delivery.body, outbox.body));
}

pub(super) fn seed_legacy_terminal_retention_rows(journal: &Journal) {
    for index in 0..35 {
        let delivery = WebhookDelivery {
            delivery_id: format!("legacy-terminal-{index}"),
            repository_id: 41,
            event: "workflow_job".to_owned(),
            headers_json: format!(r#"{{"x-legacy":"{index}"}}"#),
            body: format!("legacy-terminal-payload-{index}").into_bytes(),
            state: "received".to_owned(),
        };
        assert!(
            journal
                .persist_delivery(&delivery)
                .unwrap_or_else(|_| panic!("persist legacy terminal delivery"))
        );
        journal
            .set_delivery_state(&delivery.delivery_id, "queued", None)
            .unwrap_or_else(|_| panic!("terminalize legacy delivery"));

        let outbox = OutboxMessage {
            id: format!("legacy-outbox-{index}"),
            kind: "check".to_owned(),
            destination: "https://github.invalid".to_owned(),
            headers_json: format!(r#"{{"x-legacy":"{index}"}}"#),
            body: format!("legacy-outbox-payload-{index}").into_bytes(),
            state: "pending".to_owned(),
        };
        journal
            .enqueue_outbox(&outbox)
            .unwrap_or_else(|_| panic!("persist legacy terminal outbox"));
        journal
            .set_outbox_state(&outbox.id, "delivered", None)
            .unwrap_or_else(|_| panic!("terminalize legacy outbox"));

        let snapshot = PoolSnapshot {
            id: format!("legacy-snapshot-{index}"),
            repository_id: 41,
            request_token: format!("legacy-token-{index}"),
            config_json: format!(r#"{{"legacy_secret":"secret-{index}"}}"#),
            state: "creating".to_owned(),
            garm_pool_id: Some(format!("pool-{index}")),
            expires_at: i64::MAX as u64,
        };
        journal
            .save_pool_snapshot(&snapshot)
            .unwrap_or_else(|_| panic!("persist legacy terminal snapshot"));
        journal
            .set_pool_snapshot_state(&snapshot.id, "deleted", None)
            .unwrap_or_else(|_| panic!("terminalize legacy snapshot"));
    }
}
