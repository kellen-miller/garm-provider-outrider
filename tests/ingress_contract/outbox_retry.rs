use super::*;

#[tokio::test]
async fn failed_relays_back_off_across_restart_without_blocking_fresh_work() {
    let directory = TestDirectory::new().expect("directory");
    let garm = HttpFixture::start(FixtureMode::GarmPool)
        .await
        .expect("GARM");
    garm.state.webhook_status.store(500, Ordering::SeqCst);
    let settings =
        ingress_settings(&directory, "http://127.0.0.1:1/", &garm.origin).expect("settings");
    let config = config_with_ingress(settings);
    let journal = directory.journal().expect("journal");
    let service = IngressService::new(config.clone(), journal.clone()).expect("service");
    let connection = rusqlite::Connection::open(directory.path.join("journal.sqlite3"))
        .expect("journal inspection");
    for id in ["old", "fresh"] {
        journal
            .enqueue_outbox(&OutboxMessage {
                id: id.into(),
                kind: "forward".into(),
                destination: CONTROLLER_ID.into(),
                headers_json: r#"{"x-github-event":"workflow_job"}"#.into(),
                body: completed_failure_body(),
                state: "pending".into(),
            })
            .expect("queue");
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        service.process_pending(1).await.expect("attempt");
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let due: i64 = connection
            .query_row(
                "SELECT next_attempt_at FROM ingress_outbox WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .expect("initial retry deadline");
        assert!((before + 1..=after + 1).contains(&due));
        assert_eq!(garm.requests().len(), if id == "old" { 1 } else { 2 });
        connection
            .execute(
                "UPDATE ingress_outbox SET next_attempt_at=CAST(strftime('%s','now') AS INTEGER)+3600 WHERE id=?1",
                [id],
            )
            .expect("hold synthetic future deadline");
        assert_eq!(service.process_pending(1).await.expect("deferred"), 0);
    }

    // The old deferred row did not consume the limit when the fresh row arrived.
    assert_eq!(garm.requests().len(), 2);
    drop(service);
    drop(journal);
    let journal = directory.journal().expect("reopened journal");
    let service = IngressService::new(config, journal.clone()).expect("restarted service");
    assert_eq!(service.process_pending(1).await.expect("restart defer"), 0);
    assert_eq!(garm.requests().len(), 2);
    assert_eq!(
        journal.pending_outbox(10).expect("retained intents").len(),
        2
    );

    connection
        .execute(
            "UPDATE ingress_outbox SET next_attempt_at=0,attempts=100000 WHERE id='old'",
            [],
        )
        .expect("make old high-attempt retry due");
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    service.process_pending(1).await.expect("due retry");
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let due: i64 = connection
        .query_row(
            "SELECT next_attempt_at FROM ingress_outbox WHERE id='old'",
            [],
            |row| row.get(0),
        )
        .expect("retry deadline");
    assert!((before + 60..=after + 60).contains(&due));
    assert_eq!(garm.requests().len(), 3);
    garm.state.webhook_status.store(200, Ordering::SeqCst);
    connection
        .execute(
            "UPDATE ingress_outbox SET next_attempt_at=0 WHERE id='old'",
            [],
        )
        .expect("make recovery due");
    assert_eq!(
        service.process_pending(1).await.expect("recovered relay"),
        1
    );
    assert_eq!(
        journal.outbox_state("old").unwrap().as_deref(),
        Some("delivered")
    );
}

#[test]
fn version_fifteen_outbox_migration_preserves_delivery_intent() {
    let directory = TestDirectory::new().expect("directory");
    let journal = directory.journal().expect("journal");
    let message = OutboxMessage {
        id: "legacy".into(),
        kind: "forward".into(),
        destination: CONTROLLER_ID.into(),
        headers_json: r#"{"x-github-event":"workflow_job"}"#.into(),
        body: completed_failure_body(),
        state: "pending".into(),
    };
    journal.enqueue_outbox(&message).expect("enqueue");
    drop(journal);
    let connection = rusqlite::Connection::open(directory.path.join("journal.sqlite3"))
        .expect("previous schema");
    connection
        .execute_batch(
            "DROP INDEX ingress_outbox_due;
             ALTER TABLE ingress_outbox DROP COLUMN next_attempt_at;
             UPDATE schema_version SET version=15;",
        )
        .expect("model deployed version15");
    drop(connection);
    let journal = directory.journal().expect("upgrade");
    let pending = journal.pending_outbox(1).expect("retained outbox");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, message.id);
    assert_eq!(pending[0].body, message.body);
    assert_eq!(pending[0].headers_json, message.headers_json);
}
