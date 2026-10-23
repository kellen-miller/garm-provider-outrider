use super::rerun_state::rerun_reservation;
use super::*;

#[tokio::test]
async fn compacted_terminal_pool_replay_is_rejected_without_dependency_retry() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(push_run(), job_response(), true)
        .await
        .expect("GitHub API fixture");
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM API fixture");
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let service = IngressService::new(full_ingress_config(settings), journal.clone())
        .expect("production ingress service");
    let snapshot = PoolSnapshot {
        id: "compacted-terminal-pool".to_owned(),
        repository_id: REPOSITORY_ID,
        request_token: TOKEN.to_owned(),
        config_json: "{}".to_owned(),
        state: "deleted".to_owned(),
        garm_pool_id: Some("terminal-pool".to_owned()),
        expires_at: i64::MAX as u64,
    };
    journal
        .save_pool_snapshot(&snapshot)
        .expect("terminal snapshot");
    let connection = rusqlite::Connection::open(directory.path.join("journal.sqlite3"))
        .expect("open fixture database");
    connection
        .execute(
            "UPDATE pool_snapshots SET terminal_at=1 WHERE id=?1",
            [&snapshot.id],
        )
        .expect("age terminal snapshot");
    drop(connection);
    journal
        .maintain_retention(0, 32)
        .expect("compact terminal snapshot");
    assert!(
        journal
            .pool_snapshot_by_id(&snapshot.id)
            .expect("read compacted snapshot")
            .expect("terminal snapshot exists")
            .config_json
            .contains("compacted")
    );

    let queued = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "compacted-terminal-replay", &queued);
    service
        .intake(
            "workflow_job",
            "compacted-terminal-replay",
            &signature,
            headers,
            &queued,
        )
        .expect("durably accept replay before worker processing");
    service
        .process_pending(10)
        .await
        .expect("reject replay against terminal compacted pool");
    assert_eq!(
        journal
            .delivery_state("compacted-terminal-replay")
            .expect("read replay delivery"),
        Some("queued".to_owned())
    );
    assert!(
        garm.requests()
            .iter()
            .all(|request| request.method != "POST" || !request.path.ends_with("/pools"))
    );
}

#[tokio::test]
async fn signed_cancellation_veto_survives_reopen_and_lagging_run_api() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(failed_run(), failed_assigned_job(), true)
        .await
        .expect("GitHub API fixture");
    assert_eq!(
        github
            .state
            .github_run
            .as_ref()
            .expect("lagging workflow run")
            .get("status")
            .and_then(Value::as_str),
        Some("in_progress")
    );
    let journal = directory.journal().expect("journal");
    let settings =
        ingress_settings(&directory, &github.origin, "http://127.0.0.1:1/").expect("settings");
    seed_interrupted_assignment(&journal, &settings);
    let service = IngressService::new(enabled_rerun_config(settings.clone()), journal.clone())
        .expect("rerun-enabled ingress");

    let completion = completed_failure_body();
    let (completion_signature, completion_headers) =
        webhook("workflow_job", "cancel-veto-job-complete", &completion);
    service
        .intake(
            "workflow_job",
            "cancel-veto-job-complete",
            &completion_signature,
            completion_headers,
            &completion,
        )
        .expect("signed failed job completion");
    service
        .process_pending(10)
        .await
        .expect("persist authenticated failed job");

    let run_event = |conclusion: &str| {
        json!({
            "action":"completed",
            "repository":{"id":REPOSITORY_ID,"name":"base-repo","full_name":"base-owner/base-repo","owner":{"login":"base-owner"}},
            "workflow_run":{"id":201,"run_attempt":1,"head_sha":SHA,"status":"completed","conclusion":conclusion}
        })
        .to_string()
        .into_bytes()
    };
    let cancelled = run_event("cancelled");
    let (cancel_signature, cancel_headers) =
        webhook("workflow_run", "cancel-veto-cancelled", &cancelled);
    service
        .intake(
            "workflow_run",
            "cancel-veto-cancelled",
            &cancel_signature,
            cancel_headers,
            &cancelled,
        )
        .expect("signed cancellation intake");
    assert!(
        journal
            .run_is_cancelled(REPOSITORY_ID, 201, 1)
            .expect("cancel veto")
    );

    let later_success = run_event("success");
    let (success_signature, success_headers) =
        webhook("workflow_run", "cancel-veto-late-success", &later_success);
    service
        .intake(
            "workflow_run",
            "cancel-veto-late-success",
            &success_signature,
            success_headers,
            &later_success,
        )
        .expect("late completion intake");
    assert!(
        journal
            .run_is_cancelled(REPOSITORY_ID, 201, 1)
            .expect("monotonic cancel veto")
    );

    drop(service);
    drop(journal);
    let reopened = directory.journal().expect("reopened journal");
    assert!(
        reopened
            .run_is_cancelled(REPOSITORY_ID, 201, 1)
            .expect("reopened cancel veto")
    );
    let service = IngressService::new(enabled_rerun_config(settings), reopened.clone())
        .expect("reopened rerun-enabled ingress");
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(10)
            .await
            .expect("lagging in-progress API is vetoed"),
        0
    );
    let requests = github.requests();
    assert!(requests.iter().any(|request| {
        request.method == "GET" && request.path == "/repos/base-owner/base-repo/actions/runs/201"
    }));
    assert!(
        requests
            .iter()
            .all(|request| !(request.method == "POST" && request.path.ends_with("/rerun")))
    );
    assert!(
        reopened
            .pending_job_reruns(10)
            .expect("no cancellation intent")
            .is_empty()
    );
    assert!(
        !reopened
            .reserve_job_rerun(&rerun_reservation(&github.origin, 1, 302, 1))
            .expect("cancellation vetoes intent reservation")
    );
    assert!(
        reopened
            .pending_job_reruns(10)
            .expect("reservation did not consume run cap")
            .is_empty()
    );
}

#[tokio::test]
async fn terminal_during_pool_create_never_enables_or_forwards() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(push_run(), job_response(), true)
        .await
        .expect("GitHub API fixture");
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM API fixture");
    garm.state
        .terminal_during_pool_create
        .store(true, Ordering::SeqCst);
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let service = IngressService::new(full_ingress_config(settings), journal.clone())
        .expect("production ingress service");
    let body = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "delivery-terminal-create", &body);
    service
        .intake(
            "workflow_job",
            "delivery-terminal-create",
            &signature,
            headers,
            &body,
        )
        .expect("durable intake");

    let _processed = service
        .process_pending(1)
        .await
        .expect("terminal during create is reconciled");
    assert!(
        journal
            .run_is_terminal(REPOSITORY_ID, 201, 1)
            .expect("terminal marker from create fixture")
    );
    let snapshot = journal
        .pool_snapshot(REPOSITORY_ID, TOKEN)
        .expect("snapshot query")
        .expect("prepared snapshot");
    assert_eq!(snapshot.state, "deleted");
    let pool = garm
        .state
        .created_pool
        .lock()
        .expect("pool fixture lock")
        .clone()
        .expect("created remote pool");
    assert_eq!(pool["enabled"], false);
    assert!(garm.requests().iter().all(|request| {
        !(request.method == "PUT"
            && request.path.ends_with("/pool-201")
            && serde_json::from_slice::<Value>(&request.body)
                .is_ok_and(|body| body["enabled"] == true))
    }));
    assert!(
        garm.requests()
            .iter()
            .all(|request| { request.path != "/webhooks/00000000-0000-0000-0000-000000000901" })
    );
}

#[tokio::test]
async fn restart_after_enable_before_local_ack_disables_terminal_pool() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(push_run(), job_response(), true)
        .await
        .expect("GitHub API fixture");
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start(FixtureMode::GarmPool)
        .await
        .expect("GARM API fixture");
    garm.state
        .fail_enable_after_store
        .store(true, Ordering::SeqCst);
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let config = full_ingress_config(settings);
    let service = IngressService::new(config.clone(), journal.clone()).expect("ingress service");
    let body = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "delivery-enable-crash", &body);
    service
        .intake(
            "workflow_job",
            "delivery-enable-crash",
            &signature,
            headers,
            &body,
        )
        .expect("durable intake");
    assert_eq!(service.process_pending(1).await.expect("remote enable"), 0);
    assert_eq!(
        journal
            .delivery_state("delivery-enable-crash")
            .expect("delivery state")
            .as_deref(),
        Some("retry")
    );
    let pool = garm
        .state
        .created_pool
        .lock()
        .expect("pool fixture lock")
        .clone()
        .expect("remote pool persisted");
    assert_eq!(pool["enabled"], true);
    journal
        .mark_run_terminal(REPOSITORY_ID, 201, 1)
        .expect("completion races local acknowledgment");
    drop(service);
    drop(journal);

    let recovered = directory.journal().expect("restart journal");
    let restarted =
        IngressService::new(config, recovered.clone()).expect("restarted ingress service");
    let _processed = restarted
        .process_pending(1)
        .await
        .expect("restart disables and drains terminal pool");
    let pool = garm
        .state
        .created_pool
        .lock()
        .expect("pool fixture lock")
        .clone()
        .expect("remote pool state");
    assert_eq!(pool["enabled"], false);
    assert_eq!(
        recovered
            .pool_snapshot(REPOSITORY_ID, TOKEN)
            .expect("snapshot query")
            .expect("durable snapshot")
            .state,
        "deleted"
    );
    assert!(
        garm.requests()
            .iter()
            .all(|request| { request.path != "/webhooks/00000000-0000-0000-0000-000000000901" })
    );
}

#[tokio::test]
async fn remote_pool_success_before_local_ack_survives_restart() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(push_run(), job_response(), true)
        .await
        .expect("GitHub API fixture");
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start(FixtureMode::GarmPool)
        .await
        .expect("GARM API fixture");
    garm.state
        .fail_pool_list_after_create
        .store(true, Ordering::SeqCst);
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let config = full_ingress_config(settings);
    let service = IngressService::new(config.clone(), journal.clone()).expect("ingress service");
    let body = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "delivery-remote-commit", &body);
    let _inserted = service
        .intake(
            "workflow_job",
            "delivery-remote-commit",
            &signature,
            headers,
            &body,
        )
        .expect("intake");
    assert_eq!(service.process_pending(1).await.expect("first attempt"), 0);
    assert_eq!(
        journal
            .delivery_state("delivery-remote-commit")
            .expect("delivery state")
            .as_deref(),
        Some("retry")
    );
    assert_eq!(
        garm.requests()
            .iter()
            .filter(|request| request.method == "POST" && request.path.ends_with("/pools"))
            .count(),
        1
    );
    assert_eq!(
        journal
            .pool_snapshot(REPOSITORY_ID, TOKEN)
            .expect("snapshot")
            .expect("frozen config")
            .state,
        "creating"
    );
    drop(service);
    drop(journal);

    let recovered = directory.journal().expect("restart journal");
    let restarted =
        IngressService::new(config, recovered.clone()).expect("restarted ingress service");
    assert_eq!(
        restarted
            .process_pending(1)
            .await
            .expect("reconcile after restart"),
        2
    );
    assert_eq!(
        recovered
            .delivery_state("delivery-remote-commit")
            .expect("delivery state")
            .as_deref(),
        Some("queued")
    );
    assert_eq!(
        recovered
            .pool_snapshot(REPOSITORY_ID, TOKEN)
            .expect("snapshot")
            .expect("snapshot")
            .state,
        "enabled"
    );
    let requests = garm.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST" && request.path.ends_with("/pools"))
            .count(),
        1
    );
    assert!(requests.iter().any(|request| request.path
        == "/webhooks/00000000-0000-0000-0000-000000000901"
        && request.body == body));
}

#[tokio::test]
async fn ambiguous_pool_create_is_never_posted_twice_on_empty_inventory() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(push_run(), job_response(), true)
        .await
        .expect("GitHub API fixture");
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start(FixtureMode::GarmPool)
        .await
        .expect("GARM API fixture");
    garm.state
        .force_empty_pool_inventory
        .store(true, Ordering::SeqCst);
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let config = full_ingress_config(settings);
    let service = IngressService::new(config.clone(), journal.clone()).expect("ingress service");
    let body = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "delivery-ambiguous-create", &body);
    service
        .intake(
            "workflow_job",
            "delivery-ambiguous-create",
            &signature,
            headers,
            &body,
        )
        .expect("durable intake");
    assert_eq!(
        service.process_pending(1).await.expect("ambiguous create"),
        0
    );
    assert_eq!(
        journal
            .pool_snapshot(REPOSITORY_ID, TOKEN)
            .expect("snapshot query")
            .expect("create intent")
            .state,
        "creating"
    );
    drop(service);
    drop(journal);

    let recovered = directory.journal().expect("restart journal");
    let restarted =
        IngressService::new(config, recovered.clone()).expect("restarted ingress service");
    assert_eq!(
        restarted
            .process_pending(1)
            .await
            .expect("empty inventory remains ambiguous"),
        0
    );
    assert_eq!(
        garm.requests()
            .iter()
            .filter(|request| request.method == "POST" && request.path.ends_with("/pools"))
            .count(),
        1
    );
}
