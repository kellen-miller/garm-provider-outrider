use super::*;

#[tokio::test]
async fn stale_queued_delivery_does_not_relay_progressed_job() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(push_run(), job_response(), true)
        .await
        .expect("GitHub API fixture");
    let mut progressed = job_response();
    progressed["status"] = json!("in_progress");
    progressed["runner_name"] = json!("runner-301");
    progressed["runner_id"] = json!(9001);
    progressed["started_at"] = json!("2026-10-21T00:00:00Z");
    github.set_github_job(progressed.clone());
    let mut listed = job_response();
    listed["status"] = json!("queued");
    github.set_github_job_list(listed);
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM API fixture");
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let service = IngressService::new(full_ingress_config(settings), journal.clone())
        .expect("production ingress service");
    let body = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "delivery-stale-queued", &body);
    service
        .intake(
            "workflow_job",
            "delivery-stale-queued",
            &signature,
            headers,
            &body,
        )
        .expect("durable intake");

    assert_eq!(
        service.process_pending(1).await.expect("process delivery"),
        1
    );
    assert!(journal.pending_outbox(10).expect("outbox").is_empty());
    assert!(
        journal
            .active_pool_snapshots()
            .expect("snapshots")
            .is_empty()
    );
    assert!(garm.requests().is_empty());
    assert!(github.state.checks.lock().expect("checks").is_empty());
    let observation = journal
        .job_observation(REPOSITORY_ID, 201, 1, 301)
        .expect("observation query")
        .expect("authenticated observation");
    assert_eq!(observation.status, "in_progress");
    assert_eq!(observation.runner_name.as_deref(), Some("runner-301"));
}

#[tokio::test]
async fn queued_api_does_not_regress_a_persisted_running_job() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(push_run(), job_response(), true)
        .await
        .expect("GitHub API fixture");
    let journal = directory.journal().expect("journal");
    journal
        .record_job_observation(&JobObservationRecord {
            repository_id: REPOSITORY_ID,
            repository_owner: "base-owner".into(),
            repository_name: "base-repo".into(),
            run_id: 201,
            run_attempt: 1,
            job_id: 301,
            head_sha: SHA.into(),
            status: "in_progress".into(),
            conclusion: None,
            run_status: "in_progress".into(),
            run_conclusion: None,
            runner_name: Some("runner-301".into()),
            runner_id: Some(9001),
            started_at: Some("2026-10-21T00:00:00Z".into()),
            completed_at: None,
        })
        .expect("persist prior progress");
    let garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM API fixture");
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let service = IngressService::new(full_ingress_config(settings), journal.clone())
        .expect("production ingress service");
    let body = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "delivery-api-regressed", &body);
    service
        .intake(
            "workflow_job",
            "delivery-api-regressed",
            &signature,
            headers,
            &body,
        )
        .expect("durable intake");

    service.process_pending(1).await.expect("process delivery");
    assert_eq!(
        journal
            .job_observation(REPOSITORY_ID, 201, 1, 301)
            .expect("observation query")
            .expect("observation")
            .status,
        "in_progress"
    );
    assert!(journal.pending_outbox(10).expect("outbox").is_empty());
    assert!(garm.requests().is_empty());
}

#[tokio::test]
async fn waiting_delivery_never_admits_a_pool() {
    let directory = TestDirectory::new().expect("temp directory");
    let mut waiting = job_response();
    waiting["status"] = json!("waiting");
    let github = HttpFixture::start_github(push_run(), waiting, true)
        .await
        .expect("GitHub API fixture");
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM API fixture");
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let service = IngressService::new(full_ingress_config(settings), journal.clone())
        .expect("production ingress service");
    let body = json!({
        "action":"waiting",
        "repository":{"id":101,"name":"base-repo","full_name":"base-owner/base-repo","owner":{"login":"base-owner"}},
        "workflow_job":{"id":301,"run_id":201,"run_attempt":1,"head_sha":SHA,"status":"waiting","labels":[TOKEN]}
    })
    .to_string()
    .into_bytes();
    let (signature, headers) = webhook("workflow_job", "delivery-waiting", &body);
    service
        .intake(
            "workflow_job",
            "delivery-waiting",
            &signature,
            headers,
            &body,
        )
        .expect("durable intake");

    service.process_pending(1).await.expect("process delivery");
    assert!(
        journal
            .active_pool_snapshots()
            .expect("snapshots")
            .is_empty()
    );
    assert!(garm.requests().iter().all(|request| {
        !request.method.eq_ignore_ascii_case("POST") || !request.path.ends_with("/pools")
    }));
    assert!(github.state.checks.lock().expect("checks").is_empty());

    let mut queued = job_response();
    queued["status"] = json!("queued");
    github.set_github_job(queued);
    let queued = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "delivery-approved-queued", &queued);
    service
        .intake(
            "workflow_job",
            "delivery-approved-queued",
            &signature,
            headers,
            &queued,
        )
        .expect("durable approved delivery");
    service
        .process_pending(1)
        .await
        .expect("process approved job");
    assert_eq!(
        journal
            .pool_snapshot(REPOSITORY_ID, TOKEN)
            .expect("snapshot query")
            .expect("admitted after approval")
            .state,
        "enabled"
    );
}

#[tokio::test]
async fn completed_run_barrier_rejects_lagging_queued_job() {
    let directory = TestDirectory::new().expect("temp directory");
    let mut completed_run = push_run();
    completed_run["status"] = json!("completed");
    completed_run["conclusion"] = json!("success");
    let github = HttpFixture::start_github(completed_run, job_response(), true)
        .await
        .expect("GitHub API fixture");
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM API fixture");
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let service = IngressService::new(full_ingress_config(settings), journal.clone())
        .expect("production ingress service");
    for delivery_id in ["delivery-run-terminal", "delivery-run-terminal-replay"] {
        let body = queued_body(vec![TOKEN]);
        let (signature, headers) = webhook("workflow_job", delivery_id, &body);
        service
            .intake("workflow_job", delivery_id, &signature, headers, &body)
            .expect("durable intake");
        service.process_pending(1).await.expect("process delivery");
    }

    assert!(
        journal
            .run_is_terminal(REPOSITORY_ID, 201, 1)
            .expect("terminal run")
    );
    assert!(
        journal
            .active_pool_snapshots()
            .expect("snapshots")
            .is_empty()
    );
    assert!(journal.pending_outbox(10).expect("outbox").is_empty());
    assert!(garm.requests().is_empty());
    assert!(github.state.checks.lock().expect("checks").is_empty());
}

#[tokio::test]
async fn exact_terminal_attempt_barriers_completed_job_without_current_run_refresh() {
    let directory = TestDirectory::new().expect("temp directory");
    let mut terminal_run = push_run();
    terminal_run["status"] = json!("completed");
    terminal_run["conclusion"] = json!("success");
    let github = HttpFixture::start_github(terminal_run, failed_assigned_job(), true)
        .await
        .expect("GitHub API fixture");
    let mut lagging_current = push_run();
    lagging_current["status"] = json!("in_progress");
    github.set_current_run(lagging_current);
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM API fixture");
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let service = IngressService::new(full_ingress_config(settings), journal.clone())
        .expect("production ingress service");
    let body = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "delivery-exact-terminal-run", &body);
    service
        .intake(
            "workflow_job",
            "delivery-exact-terminal-run",
            &signature,
            headers,
            &body,
        )
        .expect("durable intake");

    service
        .process_pending(1)
        .await
        .expect("process terminal attempt");
    assert!(
        journal
            .run_is_terminal(REPOSITORY_ID, 201, 1)
            .expect("terminal run")
    );
    assert!(
        journal
            .active_pool_snapshots()
            .expect("snapshots")
            .is_empty()
    );
    assert!(journal.pending_outbox(10).expect("outbox").is_empty());
    assert!(garm.requests().is_empty());
    assert!(github.state.checks.lock().expect("checks").is_empty());
    assert!(github.requests().iter().any(|request| {
        request.path == "/repos/base-owner/base-repo/actions/runs/201/attempts/1"
    }));
    assert!(
        !github
            .requests()
            .iter()
            .any(|request| { request.path == "/repos/base-owner/base-repo/actions/runs/201" })
    );
}

#[tokio::test]
async fn malformed_authenticated_run_state_retries_without_author_check() {
    let directory = TestDirectory::new().expect("temp directory");
    let mut malformed_run = push_run();
    malformed_run["status"] = json!("finished-ish");
    let github = HttpFixture::start_github(malformed_run, job_response(), true)
        .await
        .expect("GitHub API fixture");
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM API fixture");
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let service = IngressService::new(full_ingress_config(settings), journal.clone())
        .expect("production ingress service");
    let body = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "delivery-malformed-run-state", &body);
    service
        .intake(
            "workflow_job",
            "delivery-malformed-run-state",
            &signature,
            headers,
            &body,
        )
        .expect("durable intake");

    assert_eq!(
        service.process_pending(1).await.expect("retry dependency"),
        0
    );
    assert_eq!(
        journal.pending_deliveries(10).expect("retry inbox").len(),
        1
    );
    assert!(journal.pending_outbox(10).expect("outbox").is_empty());
    assert!(
        journal
            .active_pool_snapshots()
            .expect("snapshots")
            .is_empty()
    );
    assert!(garm.requests().is_empty());
    assert!(github.state.checks.lock().expect("checks").is_empty());
}

#[tokio::test]
async fn cancelled_run_barrier_survives_reopen_and_replay() {
    let directory = TestDirectory::new().expect("temp directory");
    let mut cancelled_run = push_run();
    cancelled_run["status"] = json!("completed");
    cancelled_run["conclusion"] = json!("cancelled");
    let github = HttpFixture::start_github(cancelled_run, job_response(), true)
        .await
        .expect("GitHub API fixture");
    let journal = directory.journal().expect("journal");
    let mut garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM API fixture");
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let service = IngressService::new(full_ingress_config(settings), journal.clone())
        .expect("production ingress service");
    let body = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "delivery-api-cancelled", &body);
    service
        .intake(
            "workflow_job",
            "delivery-api-cancelled",
            &signature,
            headers,
            &body,
        )
        .expect("durable intake");
    service.process_pending(1).await.expect("process delivery");
    drop(service);
    garm.task.abort();
    let _shutdown = (&mut garm.task).await;
    drop(garm);
    drop(journal);

    let reopened = directory.journal().expect("reopened journal");
    let replay_garm = HttpFixture::start_garm(reopened.clone())
        .await
        .expect("reopened GARM API fixture");
    let settings =
        ingress_settings(&directory, &github.origin, &replay_garm.origin).expect("settings");
    let replay_service = IngressService::new(full_ingress_config(settings), reopened.clone())
        .expect("reopened ingress service");
    let (signature, headers) = webhook("workflow_job", "delivery-api-cancelled-replay", &body);
    replay_service
        .intake(
            "workflow_job",
            "delivery-api-cancelled-replay",
            &signature,
            headers,
            &body,
        )
        .expect("durable replay intake");
    replay_service
        .process_pending(1)
        .await
        .expect("process replay after restart");

    assert!(
        reopened
            .run_is_cancelled(REPOSITORY_ID, 201, 1)
            .expect("cancelled run")
    );
    assert!(
        reopened
            .run_is_terminal(REPOSITORY_ID, 201, 1)
            .expect("terminal run")
    );
    assert!(
        reopened
            .active_pool_snapshots()
            .expect("snapshots")
            .is_empty()
    );
    assert!(reopened.pending_outbox(10).expect("outbox").is_empty());
    assert!(replay_garm.requests().is_empty());
    assert!(github.state.checks.lock().expect("checks").is_empty());
}

#[tokio::test]
async fn terminal_job_delivery_waits_for_lagging_api_state() {
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
    let body = completed_failure_body();
    let (signature, headers) = webhook("workflow_job", "delivery-terminal-ahead", &body);
    service
        .intake(
            "workflow_job",
            "delivery-terminal-ahead",
            &signature,
            headers,
            &body,
        )
        .expect("durable intake");

    assert_eq!(
        service
            .process_pending(1)
            .await
            .expect("retry pending delivery"),
        0
    );
    assert_eq!(
        journal.pending_deliveries(10).expect("retry inbox").len(),
        1
    );
    assert!(journal.pending_outbox(10).expect("outbox").is_empty());
    assert!(garm.requests().is_empty());
    assert!(github.state.checks.lock().expect("checks").is_empty());
}

#[tokio::test]
async fn contradictory_terminal_list_and_detail_reject_delivery() {
    let directory = TestDirectory::new().expect("temp directory");
    let mut detailed_success = failed_assigned_job();
    detailed_success["conclusion"] = json!("success");
    let github = HttpFixture::start_github(push_run(), detailed_success, true)
        .await
        .expect("GitHub API fixture");
    github.set_github_job_list(failed_assigned_job());
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM API fixture");
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let service = IngressService::new(full_ingress_config(settings), journal.clone())
        .expect("production ingress service");
    let body = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "delivery-conflicting-terminal-api", &body);
    service
        .intake(
            "workflow_job",
            "delivery-conflicting-terminal-api",
            &signature,
            headers,
            &body,
        )
        .expect("durable intake");

    assert_eq!(
        service.process_pending(1).await.expect("reject conflict"),
        2
    );
    assert!(journal.pending_deliveries(10).expect("inbox").is_empty());
    assert!(
        journal
            .active_pool_snapshots()
            .expect("snapshots")
            .is_empty()
    );
    assert!(journal.pending_outbox(10).expect("outbox").is_empty());
    assert!(garm.requests().is_empty());
    assert_eq!(github.state.checks.lock().expect("checks").len(), 1);
}

#[tokio::test]
async fn terminal_list_ahead_of_detail_retries_without_admission_check() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(push_run(), job_response(), true)
        .await
        .expect("GitHub API fixture");
    github.set_github_job_list(failed_assigned_job());
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM API fixture");
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let service = IngressService::new(full_ingress_config(settings), journal.clone())
        .expect("production ingress service");
    let body = queued_body(vec![TOKEN]);
    let (signature, headers) = webhook("workflow_job", "delivery-terminal-list-ahead", &body);
    service
        .intake(
            "workflow_job",
            "delivery-terminal-list-ahead",
            &signature,
            headers,
            &body,
        )
        .expect("durable intake");

    assert_eq!(
        service
            .process_pending(1)
            .await
            .expect("retry lagging detail"),
        0
    );
    assert_eq!(
        journal.pending_deliveries(10).expect("retry inbox").len(),
        1
    );
    assert!(
        journal
            .active_pool_snapshots()
            .expect("snapshots")
            .is_empty()
    );
    assert!(journal.pending_outbox(10).expect("outbox").is_empty());
    assert!(garm.requests().is_empty());
    assert!(github.state.checks.lock().expect("checks").is_empty());
}
