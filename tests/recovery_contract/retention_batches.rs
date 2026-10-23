use super::*;

#[test]
fn retention_limits_each_terminal_payload_batch() {
    let fixture = RecoveryFixture::new();
    let journal = fixture.journal();
    for index in 0..33 {
        let delivery = WebhookDelivery {
            delivery_id: format!("retention-batch-{index}"),
            repository_id: 41,
            event: "workflow_job".to_owned(),
            headers_json: "{}".to_owned(),
            body: format!("payload-{index}").into_bytes(),
            state: "received".to_owned(),
        };
        assert!(journal.persist_delivery(&delivery).unwrap());
        journal
            .set_delivery_state(&delivery.delivery_id, "queued", None)
            .unwrap();
    }
    drop(journal);
    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    connection
        .execute("UPDATE webhook_deliveries SET terminal_at=1", [])
        .unwrap();
    drop(connection);

    let journal = fixture.journal();
    assert_eq!(journal.maintain_retention(0, usize::MAX).unwrap(), 32);
    drop(journal);
    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    let retained_payloads: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM webhook_deliveries WHERE length(body)>0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained_payloads, 1);
}

pub(super) fn set_history_request(
    request: &mut outrider::garm::ProviderRequest,
    runner_name: &str,
    token: &str,
    snapshot_id: &str,
) {
    let bootstrap = request
        .bootstrap
        .as_mut()
        .unwrap_or_else(|| panic!("history policy bootstrap"));
    bootstrap.name = runner_name.to_owned();
    bootstrap.labels = vec![
        token.to_owned(),
        "runner-pool-id=pool-1".to_owned(),
        "runner-controller-id=controller-1".to_owned(),
    ];
    bootstrap.extra_specs = Some(serde_json::json!({"snapshot_id":snapshot_id}));
    request.pool_extra_specs = Some(serde_json::json!({"snapshot_id":snapshot_id}));
}
