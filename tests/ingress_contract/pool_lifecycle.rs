use super::*;

#[tokio::test]
async fn rejection_check_dedup_outage() {
    let directory = TestDirectory::new().expect("temp directory");
    let (service, journal) =
        open_service(&directory, "http://127.0.0.1:1/", "http://127.0.0.1:1/").expect("service");
    for (id, token) in [
        ("delivery-8a", "outrider:v1:101:201:1:BAD"),
        ("delivery-8b", "OUTRIDER:v1:101:201:1:build"),
    ] {
        let body = queued_body(vec![token]);
        let (signature, headers) = webhook("workflow_job", id, &body);
        let _inserted = service
            .intake("workflow_job", id, &signature, headers, &body)
            .expect("intake");
    }
    let _processed = service
        .process_pending(10)
        .await
        .expect("process rejected requests");
    let pending = journal.pending_outbox(10).expect("pending check");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].kind, "check");
    assert_eq!(pending[0].state, "retry");
}

#[tokio::test]
async fn changed_rejection_reason_keeps_first_durable_check_after_compaction() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(push_run(), job_response(), true)
        .await
        .expect("GitHub fixture");
    let settings = ingress_settings(&directory, &github.origin, "http://127.0.0.1:1/")
        .expect("ingress settings");
    let journal = directory.journal().expect("journal");
    let service = IngressService::new(config_with_ingress(settings), journal.clone())
        .expect("ingress service");

    let malformed = queued_body(vec!["outrider:v1:101:201:1:BAD"]);
    let (malformed_signature, malformed_headers) =
        webhook("workflow_job", "delivery-rejection-first", &malformed);
    assert!(
        service
            .intake(
                "workflow_job",
                "delivery-rejection-first",
                &malformed_signature,
                malformed_headers,
                &malformed,
            )
            .expect("persist first rejection")
    );
    assert_eq!(
        service.process_pending(1).await.expect("first rejection"),
        2
    );
    assert_eq!(
        journal
            .outbox_state("check:101:201:1:301")
            .unwrap()
            .as_deref(),
        Some("delivered")
    );
    assert_eq!(
        github.state.checks.lock().expect("delivered checks").len(),
        1
    );

    let connection = rusqlite::Connection::open(directory.path.join("journal.sqlite3"))
        .expect("open journal database");
    connection
        .execute(
            "UPDATE ingress_outbox SET terminal_at=1 WHERE id='check:101:201:1:301'",
            [],
        )
        .expect("age delivered check");
    drop(connection);
    assert!(journal.maintain_retention(0, 32).unwrap() > 0);
    assert!(journal.pending_outbox(1).unwrap().is_empty());

    let mismatch = queued_body(vec!["outrider:v1:999:202:1:build"]);
    let (mismatch_signature, mismatch_headers) =
        webhook("workflow_job", "delivery-rejection-later", &mismatch);
    assert!(
        service
            .intake(
                "workflow_job",
                "delivery-rejection-later",
                &mismatch_signature,
                mismatch_headers,
                &mismatch,
            )
            .expect("persist later rejection")
    );
    assert_eq!(
        service.process_pending(1).await.expect("later rejection"),
        1
    );
    assert_eq!(
        journal
            .delivery_state("delivery-rejection-later")
            .unwrap()
            .as_deref(),
        Some("queued")
    );
    assert_eq!(
        journal
            .outbox_state("check:101:201:1:301")
            .unwrap()
            .as_deref(),
        Some("delivered")
    );
    assert!(journal.pending_outbox(1).unwrap().is_empty());
    assert_eq!(
        github
            .state
            .checks
            .lock()
            .expect("checks after replay")
            .len(),
        1
    );

    let compacted: (String, Vec<u8>) =
        rusqlite::Connection::open(directory.path.join("journal.sqlite3"))
            .expect("reopen journal database")
            .query_row(
                "SELECT headers_json, body FROM ingress_outbox WHERE id='check:101:201:1:301'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read compacted check tombstone");
    assert_eq!(compacted, (String::new(), Vec::new()));
}

#[tokio::test]
async fn pool_caps_expiry_drain() {
    let directory = TestDirectory::new().expect("temp directory");
    let server = HttpFixture::start(FixtureMode::GarmPool)
        .await
        .expect("GARM fixture");
    server
        .state
        .created_pool
        .lock()
        .expect("pool fixture lock")
        .replace(json!({"id":"pool-201","enabled":true}));
    let settings =
        ingress_settings(&directory, "http://127.0.0.1:1/", &server.origin).expect("settings");
    let journal = directory.journal().expect("journal");
    let snapshot = PoolSnapshot {
        id: "expired-snapshot".into(),
        repository_id: REPOSITORY_ID,
        request_token: TOKEN.into(),
        config_json: "{}".into(),
        state: "enabled".into(),
        garm_pool_id: Some("pool-201".into()),
        expires_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs()
            + 3600,
    };
    journal.save_pool_snapshot(&snapshot).expect("snapshot");
    let (allocation, _) = journal
        .reserve_allocation(
            "allocation-201",
            "controller-201",
            "runner-201",
            "expired-snapshot",
            "pool-201",
        )
        .expect("allocation reservation");
    assert!(
        journal
            .claim_allocation_selection(&allocation.allocation_id)
            .expect("selection claim")
    );
    journal
        .insert_child_attempt(&ChildAttemptRecord {
            allocation_id: allocation.allocation_id.clone(),
            attempt_number: 1,
            state: "reserved".into(),
            offer_json: "{}".into(),
            binding_json: "{}".into(),
            detail: None,
            created_at: 1,
            dispatched_at: None,
            completed_at: None,
        })
        .expect("child attempt");
    journal
        .mark_child_dispatched(
            &allocation.allocation_id,
            1,
            &AllocationAdmission {
                snapshot_id: "expired-snapshot".into(),
                repository_id: REPOSITORY_ID,
                run_id: 201,
                run_attempt: 1,
                now: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_secs(),
            },
        )
        .expect("durable provider dispatch");
    assert_eq!(
        journal
            .active_pool_snapshots()
            .expect("active snapshots")
            .len(),
        1
    );
    let client = GarmClient::from_settings(&settings, std::time::Duration::from_secs(3))
        .expect("GARM client");
    assert!(matches!(
        client
            .delete_drained_pool(GARM_REPOSITORY_ID, "pool-201", 1)
            .await,
        Err(PoolError::Limit)
    ));
    let action = journal
        .begin_allocation_action("controller-201", "runner-201", "DeleteInstance")
        .expect("delete intent")
        .expect("allocation");
    assert!(action.acquired);
    journal
        .finish_allocation_action(
            action.action_id.expect("delete action ID"),
            "DeleteInstance",
            "complete",
            None,
        )
        .expect("finish delete intent");
    journal
        .set_allocation_state(&allocation.allocation_id, "deleted", None)
        .expect("terminal allocation state");
    assert!(
        client
            .delete_drained_pool(GARM_REPOSITORY_ID, "pool-201", 0)
            .await
            .is_ok()
    );
    assert!(
        server
            .requests()
            .iter()
            .any(|request| request.method == "PUT")
    );
    assert!(
        server
            .requests()
            .iter()
            .any(|request| request.path == "/api/v1/pools/pool-201/instances")
    );
    let _profile = profile();
}

#[tokio::test]
async fn unavailable_garm_restart() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(failed_run(), failed_assigned_job(), true)
        .await
        .expect("GitHub fixture");
    let (service, journal) =
        open_service(&directory, &github.origin, "http://127.0.0.1:1/").expect("service");
    let body = completed_failure_body();
    let (signature, headers) = webhook("workflow_job", "delivery-10", &body);
    let _inserted = service
        .intake("workflow_job", "delivery-10", &signature, headers, &body)
        .expect("intake");
    let _processed = service.process_pending(1).await.expect("offline worker");
    assert_eq!(
        journal
            .pending_outbox(10)
            .expect("pending before restart")
            .len(),
        1
    );
    drop(service);
    drop(journal);
    let recovered = directory.journal().expect("restart journal");
    let pending = recovered
        .pending_outbox(10)
        .expect("durable outbox after restart");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].body, body);
    assert_eq!(pending[0].state, "retry");
}

#[tokio::test]
async fn restart_after_remote_pool_delete_reconciles_exact_not_found() {
    let directory = TestDirectory::new().expect("temp directory");
    let server = HttpFixture::start(FixtureMode::GarmPool)
        .await
        .expect("GARM fixture");
    server
        .state
        .created_pool
        .lock()
        .expect("pool fixture lock")
        .replace(pool_response());
    server
        .state
        .delete_pool_removes_remote
        .store(true, Ordering::SeqCst);
    let settings =
        ingress_settings(&directory, "http://127.0.0.1:1/", &server.origin).expect("settings");
    let config = config_with_ingress(settings.clone());
    let journal = directory.journal().expect("journal");
    let snapshot = draining_pool_snapshot(&settings);
    journal.save_pool_snapshot(&snapshot).expect("snapshot");
    let garm = GarmClient::from_settings(&settings, Duration::from_secs(3)).expect("GARM client");
    garm.delete_drained_pool(GARM_REPOSITORY_ID, "pool-201", 0)
        .await
        .expect("remote pool delete before local acknowledgment");
    drop(garm);
    drop(journal);

    let recovered = directory.journal().expect("reopened journal");
    let ingress = IngressService::new(config, recovered.clone()).expect("restarted ingress");
    assert_eq!(
        ingress
            .drain_expired_pools(1)
            .await
            .expect("exact pool 404 reconciles deletion"),
        1
    );
    assert_eq!(
        recovered
            .pool_snapshot(REPOSITORY_ID, TOKEN)
            .expect("snapshot query")
            .expect("snapshot")
            .state,
        "deleted"
    );
    assert!(server.requests().iter().any(|request| {
        request.method == "GET"
            && request.path
                == "/api/v1/repositories/00000000-0000-0000-0000-000000000101/pools/pool-201"
    }));
    assert!(
        server
            .requests()
            .iter()
            .all(|request| { !(request.method == "POST" && request.path.ends_with("/pools")) })
    );
}

#[tokio::test]
async fn pool_detail_auth_and_outage_do_not_reconcile_absence() {
    for status in [401, 503] {
        let directory = TestDirectory::new().expect("temp directory");
        let server = HttpFixture::start(FixtureMode::GarmPool)
            .await
            .expect("GARM fixture");
        server
            .state
            .pool_detail_status
            .store(status, Ordering::SeqCst);
        let settings =
            ingress_settings(&directory, "http://127.0.0.1:1/", &server.origin).expect("settings");
        let journal = directory.journal().expect("journal");
        journal
            .save_pool_snapshot(&draining_pool_snapshot(&settings))
            .expect("snapshot");
        let ingress = IngressService::new(config_with_ingress(settings), journal.clone())
            .expect("ingress service");
        assert_eq!(
            ingress
                .drain_expired_pools(1)
                .await
                .expect("unknown pool state stays pending"),
            0
        );
        assert_eq!(
            journal
                .pool_snapshot(REPOSITORY_ID, TOKEN)
                .expect("snapshot query")
                .expect("snapshot")
                .state,
            "draining"
        );
        assert!(
            server
                .requests()
                .iter()
                .all(|request| request.method != "DELETE")
        );
    }
}
