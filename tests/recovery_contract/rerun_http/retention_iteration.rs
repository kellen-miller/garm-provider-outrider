use super::*;

pub(crate) async fn retention_runs_in_recovery_iteration_with_independent_inbox_work() {
    let fixture = RecoveryFixture::new();
    let github = github_fixture().await;
    let settings = build_settings(&fixture, &github.origin);
    let mut harness = allocation_harness::AllocationHarness::new(
        allocation_harness::AllocationHarnessOptions::default(),
    )
    .await;
    let journal = harness
        .journal
        .0
        .as_ref()
        .unwrap_or_else(|| panic!("recovery harness journal"))
        .clone();
    let allocation = Arc::new(
        harness
            .service
            .0
            .take()
            .unwrap_or_else(|| panic!("recovery harness allocation service")),
    );
    let ingress = Arc::new(
        IngressService::new(
            ConfigPatch {
                ingress: Some(settings),
                reruns: Some(RerunSettings {
                    enabled: false,
                    max_per_run: 1,
                    actions_write_permission: false,
                }),
                ..ConfigPatch::default()
            },
            journal.clone(),
        )
        .unwrap_or_else(|_| panic!("construct production ingress service")),
    );
    let terminal = WebhookDelivery {
        delivery_id: "recovery-iteration-terminal".to_owned(),
        repository_id: 7,
        event: "workflow_job".to_owned(),
        headers_json: "{\"signed\":true}".to_owned(),
        body: b"old-terminal-signed-bytes".to_vec(),
        state: "received".to_owned(),
    };
    assert!(
        journal
            .persist_delivery(&terminal)
            .unwrap_or_else(|_| panic!("persist terminal recovery delivery"))
    );
    journal
        .set_delivery_state(&terminal.delivery_id, "queued", None)
        .unwrap_or_else(|_| panic!("queue terminal recovery delivery"));
    let independent = WebhookDelivery {
        delivery_id: "recovery-iteration-independent".to_owned(),
        repository_id: 7,
        event: "workflow_job".to_owned(),
        headers_json: "{}".to_owned(),
        body: b"malformed event, reject without dependency".to_vec(),
        state: "received".to_owned(),
    };
    assert!(
        journal
            .persist_delivery(&independent)
            .unwrap_or_else(|_| panic!("persist independent recovery delivery"))
    );
    let database = harness._fixture.directory.join("journal.sqlite3");
    let connection = rusqlite::Connection::open(&database)
        .unwrap_or_else(|_| panic!("age terminal recovery delivery"));
    connection
        .execute(
            "UPDATE webhook_deliveries SET terminal_at=1 WHERE delivery_id=?1",
            [&terminal.delivery_id],
        )
        .unwrap_or_else(|_| panic!("age terminal recovery delivery"));
    drop(connection);

    let recovery = RecoveryService::new(allocation, ingress, journal.clone());
    recovery
        .startup()
        .unwrap_or_else(|_| panic!("start production recovery"));
    let progressed = recovery
        .iteration(32)
        .await
        .unwrap_or_else(|_| panic!("run production recovery iteration"));
    assert!(
        progressed >= 2,
        "maintenance and independent inbox work progress"
    );
    assert_eq!(
        journal
            .delivery_state(&independent.delivery_id)
            .unwrap_or_else(|_| panic!("read independent recovery delivery"))
            .as_deref(),
        Some("rejected")
    );
    assert_eq!(
        journal
            .delivery_state(&terminal.delivery_id)
            .unwrap_or_else(|_| panic!("read terminal recovery delivery"))
            .as_deref(),
        Some("queued")
    );
    let terminal_body_length: i64 = rusqlite::Connection::open(&database)
        .unwrap_or_else(|_| panic!("read recovered terminal delivery"))
        .query_row(
            "SELECT length(body) FROM webhook_deliveries WHERE delivery_id=?1",
            [&terminal.delivery_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| panic!("read recovered terminal payload length"));
    assert_eq!(terminal_body_length, 0);

    drop(recovery);
    drop(journal);
    harness.journal.0.take();
    let reopened = harness._fixture.journal();
    assert_eq!(
        reopened
            .delivery_state(&independent.delivery_id)
            .unwrap_or_else(|_| panic!("read reopened independent recovery delivery"))
            .as_deref(),
        Some("rejected")
    );
    let terminal_payload_length: i64 = rusqlite::Connection::open(&database)
        .unwrap_or_else(|_| panic!("reopen recovered terminal delivery"))
        .query_row(
            "SELECT length(body) FROM webhook_deliveries WHERE delivery_id=?1",
            [&terminal.delivery_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| panic!("reopen compacted terminal payload"));
    assert_eq!(terminal_payload_length, 0);
    drop(reopened);
}
