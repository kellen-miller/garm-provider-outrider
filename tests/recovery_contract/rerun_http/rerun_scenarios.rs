use super::*;

pub(crate) async fn default_off_and_caps() {
    let fixture = RecoveryFixture::new();
    let github = github_fixture().await;
    let journal = Arc::new(fixture.journal());
    let settings = build_settings(&fixture, &github.origin);
    seed_job_and_assignment(&fixture, &journal, &settings, Some("2020-01-01T00:00:30Z"));
    let service = build_service(settings, journal.clone(), false);
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(10)
            .await
            .unwrap_or_else(|_| panic!("default-off scan")),
        0
    );
    assert!(
        github
            .state
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("requests"))
            .is_empty()
    );
    assert!(
        journal
            .reserve_job_rerun(&reservation(&github.origin, 1, JOB_ID, 2))
            .unwrap_or_else(|_| panic!("first reservation"))
    );
    assert!(
        journal
            .reserve_job_rerun(&reservation(&github.origin, 1, JOB_ID + 1, 2))
            .unwrap_or_else(|_| panic!("second job reservation"))
    );
    assert!(
        !journal
            .reserve_job_rerun(&reservation(&github.origin, 2, JOB_ID + 2, 2))
            .unwrap_or_else(|_| panic!("stable run cap"))
    );
}

pub(crate) fn reservation(
    github_api_origin: &str,
    run_attempt: u32,
    job_id: u64,
    max_per_run: u32,
) -> JobRerunReservation {
    JobRerunReservation {
        repository_id: REPOSITORY_ID,
        owner: OWNER.to_owned(),
        repository: REPOSITORY.to_owned(),
        github_api_origin: github_api_origin.to_owned(),
        run_id: RUN_ID,
        run_attempt,
        job_id,
        max_per_run,
    }
}

pub(crate) async fn per_job_endpoint_after_completion() {
    let fixture = RecoveryFixture::new();
    let github = github_fixture().await;
    let settings = build_settings(&fixture, &github.origin);
    let journal = Arc::new(fixture.journal());
    let allocation_id = seed_job_and_assignment(&fixture, &journal, &settings, None);
    let service = build_service(settings.clone(), journal.clone(), true);
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(1)
            .await
            .unwrap_or_else(|_| panic!("early scan")),
        0
    );
    drop(service);
    drop(journal);
    let journal = Arc::new(fixture.journal());
    let settings = build_settings(&fixture, &github.origin);
    seed_late_interruption(&journal, &allocation_id);
    let service = build_service(settings.clone(), journal.clone(), true);
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(1)
            .await
            .unwrap_or_else(|_| panic!("restarted eligibility scan")),
        1
    );
    seed_job_and_assignment_for_attempt(
        &journal,
        &settings,
        Some("2020-01-01T00:00:30Z"),
        RerunAssignmentFixture {
            run_id: RUN_ID,
            run_attempt: 1,
            job_id: JOB_ID + 1,
            snapshot_job_id: JOB_ID,
            snapshot_id: "snapshot-rerun-job-302",
            allocation_request: "allocation-rerun-job-302",
            source_id: "recovery-rerun-job-302",
            runner_name: "runner-rerun-302",
        },
    );
    github.state.job_id.store(JOB_ID + 1, Ordering::SeqCst);
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(1)
            .await
            .unwrap_or_else(|_| panic!("second run slot")),
        1
    );
    seed_job_and_assignment_for_attempt(
        &journal,
        &settings,
        Some("2020-01-01T00:00:30Z"),
        RerunAssignmentFixture {
            run_id: RUN_ID,
            run_attempt: 1,
            job_id: JOB_ID + 2,
            snapshot_job_id: JOB_ID + 2,
            snapshot_id: "snapshot-rerun-job-303",
            allocation_request: "allocation-rerun-job-303",
            source_id: "recovery-rerun-job-303",
            runner_name: "runner-rerun-303",
        },
    );
    github.state.job_id.store(JOB_ID + 2, Ordering::SeqCst);
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(1)
            .await
            .unwrap_or_else(|_| panic!("bounded run cap")),
        0
    );
    let requests = github
        .state
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("requests"));
    assert!(
        requests
            .iter()
            .any(|request| request == "POST /repos/example/outrider/actions/jobs/301/rerun")
    );
    assert!(
        requests
            .iter()
            .any(|request| request == "POST /repos/example/outrider/actions/jobs/302/rerun")
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.ends_with("/rerun"))
            .count(),
        2
    );
    assert_eq!(
        journal
            .pending_job_reruns(4)
            .unwrap_or_else(|_| panic!("persisted acknowledged rerun"))[0]
            .state,
        "acknowledged"
    );
}

pub(crate) fn seed_late_interruption(journal: &outrider::journal::Journal, allocation_id: &str) {
    let identity = PlacementIdentity {
        controller_id: "00000000-0000-0000-0000-000000000901".to_owned(),
        cloud: "aws".to_owned(),
        account_id: "account".to_owned(),
        location: "us-east-1".to_owned(),
        resource_id: "instance-runner-rerun".to_owned(),
        host_id: None,
        dependency_ids: std::collections::BTreeMap::new(),
    };
    let identity_json =
        serde_json::to_string(&identity).unwrap_or_else(|_| panic!("encode identity"));
    journal
        .record_interruption(
            &InterruptionRecord {
                allocation_id: allocation_id.to_owned(),
                controller_id: "00000000-0000-0000-0000-000000000901".to_owned(),
                pool_id: "pool-rerun".to_owned(),
                catalog_fingerprint: "catalog".to_owned(),
                configuration_fingerprint: "config".to_owned(),
                provider_pin_json: "{}".to_owned(),
                identity_json,
                source_api: "fixture".to_owned(),
                api_version: "v1".to_owned(),
                source_id: "source".to_owned(),
                event_id: "event".to_owned(),
                occurred_at: "2020-01-01T00:00:30Z".to_owned(),
                observed_at: "2020-01-01T00:00:31Z".to_owned(),
                reason_fields_json: "{}".to_owned(),
            },
            &identity,
        )
        .unwrap_or_else(|_| panic!("record late interruption evidence"));
}

pub(crate) async fn ambiguous_rerun_observation() {
    let fixture = RecoveryFixture::new();
    let github = github_fixture().await;
    github.state.rerun_status.store(500, Ordering::SeqCst);
    let settings = build_settings(&fixture, &github.origin);
    let journal = Arc::new(fixture.journal());
    seed_job_and_assignment(&fixture, &journal, &settings, Some("2020-01-01T00:00:30Z"));
    let service = build_service(settings.clone(), journal.clone(), true);
    assert!(service.rerun_completed_interrupted_jobs(1).await.is_err());
    drop(service);
    drop(journal);
    let journal = Arc::new(fixture.journal());
    let changed_origin = github_fixture().await;
    let mut settings = settings;
    settings.github_api_origin = changed_origin.origin.clone();
    let service = build_service(settings, journal.clone(), true);
    service
        .process_pending(2)
        .await
        .unwrap_or_else(|_| panic!("fence changed transport"));
    assert_eq!(
        github
            .state
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("requests"))
            .iter()
            .filter(|request| request.ends_with("/rerun"))
            .count(),
        1
    );
    assert!(
        changed_origin
            .state
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("changed-origin requests"))
            .is_empty()
    );
    assert!(
        journal
            .pending_job_reruns(4)
            .unwrap_or_else(|_| panic!("fenced ambiguous request"))
            .is_empty()
    );
}

pub(crate) async fn lost_rerun_response_keeps_intent_after_restart() {
    let fixture = RecoveryFixture::new();
    let github = github_fixture().await;
    github
        .state
        .drop_rerun_response
        .store(true, Ordering::SeqCst);
    let settings = build_settings(&fixture, &github.origin);
    let journal = Arc::new(fixture.journal());
    seed_job_and_assignment(&fixture, &journal, &settings, Some("2020-01-01T00:00:30Z"));
    let service = build_service(settings.clone(), journal.clone(), true);
    assert!(service.rerun_completed_interrupted_jobs(1).await.is_err());
    let pending = journal
        .pending_job_reruns(4)
        .unwrap_or_else(|_| panic!("lost response retains intent"));
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].state, "intent");
    assert!(
        !journal
            .reserve_job_rerun(&reservation(&github.origin, 1, JOB_ID + 1, 1))
            .unwrap_or_else(|_| panic!("uncertain POST retains cap"))
    );
    drop(service);
    drop(journal);

    let reopened = Arc::new(fixture.journal());
    let service = build_service(settings, reopened.clone(), true);
    service
        .process_pending(4)
        .await
        .unwrap_or_else(|_| panic!("observe unresolved intent after restart"));
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(1)
            .await
            .unwrap_or_else(|_| panic!("uncertain attempt stays deduplicated")),
        0
    );
    let pending = reopened
        .pending_job_reruns(4)
        .unwrap_or_else(|_| panic!("reopened unknown outcome"));
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].state, "intent");
    assert_eq!(
        github
            .state
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("recorded POST requests"))
            .iter()
            .filter(|request| request.ends_with("/rerun"))
            .count(),
        1
    );
}

pub(crate) async fn forbidden_rerun_is_terminal_after_restart() {
    let fixture = RecoveryFixture::new();
    let github = github_fixture().await;
    github.state.rerun_status.store(403, Ordering::SeqCst);
    let settings = build_settings(&fixture, &github.origin);
    let journal = Arc::new(fixture.journal());
    seed_job_and_assignment(&fixture, &journal, &settings, Some("2020-01-01T00:00:30Z"));
    let service = build_service(settings.clone(), journal.clone(), true);
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(1)
            .await
            .unwrap_or_else(|_| panic!("record definitive denial")),
        0
    );
    assert!(
        journal
            .pending_job_reruns(4)
            .unwrap_or_else(|_| panic!("denied intent is terminal"))
            .is_empty()
    );
    assert!(
        !journal
            .reserve_job_rerun(&reservation(&github.origin, 1, JOB_ID + 1, 1))
            .unwrap_or_else(|_| panic!("denied POST consumes run cap"))
    );
    let requests_before_restart = github
        .state
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request log"))
        .clone();
    assert_eq!(
        requests_before_restart
            .iter()
            .filter(|request| request.ends_with("/rerun"))
            .count(),
        1
    );
    drop(service);
    drop(journal);

    let journal = Arc::new(fixture.journal());
    let service = build_service(settings, journal.clone(), true);
    service
        .process_pending(4)
        .await
        .unwrap_or_else(|_| panic!("reopen without polling denied intent"));
    assert_eq!(
        github
            .state
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("request log after restart"))
            .as_slice(),
        requests_before_restart.as_slice()
    );
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(1)
            .await
            .unwrap_or_else(|_| panic!("dedup denied job after restart")),
        0
    );
    assert_eq!(
        github
            .state
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("request log after rerun scan"))
            .iter()
            .filter(|request| request.ends_with("/rerun"))
            .count(),
        1
    );
}

pub(crate) async fn rerun_denial_observations() {
    for scenario in [1, 2, 3, 4, 5, 6, 7, 8] {
        let fixture = RecoveryFixture::new();
        let github = github_fixture().await;
        github.state.scenario.store(scenario, Ordering::SeqCst);
        if scenario == 8 {
            github.state.latest_attempt.store(2, Ordering::SeqCst);
        }
        let settings = build_settings(&fixture, &github.origin);
        let journal = Arc::new(fixture.journal());
        let interruption_at = if scenario == 3 {
            "2019-12-31T23:59:59.500Z"
        } else {
            "2020-01-01T00:00:30Z"
        };
        seed_job_and_assignment(&fixture, &journal, &settings, Some(interruption_at));
        let service = build_service(settings, journal.clone(), true);
        if let Ok(count) = service.rerun_completed_interrupted_jobs(1).await {
            assert_eq!(count, 0, "scenario {scenario} must be denied");
        }
        if scenario == 4 {
            assert!(
                journal
                    .run_is_cancelled(REPOSITORY_ID, RUN_ID, 1)
                    .unwrap_or_else(|_| panic!("persist current API cancellation"))
            );
        }
        assert!(
            github
                .state
                .requests
                .lock()
                .unwrap_or_else(|_| panic!("requests"))
                .iter()
                .all(|request| !request.ends_with("/rerun"))
        );
    }
}

pub(crate) async fn rerun_waits_for_job_completion() {
    let fixture = RecoveryFixture::new();
    let github = github_fixture().await;
    let settings = build_settings(&fixture, &github.origin);
    let journal = Arc::new(fixture.journal());
    journal
        .record_job_observation(&JobObservationRecord {
            repository_id: REPOSITORY_ID,
            repository_owner: OWNER.to_owned(),
            repository_name: REPOSITORY.to_owned(),
            run_id: RUN_ID,
            run_attempt: 1,
            job_id: JOB_ID,
            head_sha: SHA.to_owned(),
            status: "in_progress".to_owned(),
            conclusion: None,
            run_status: "in_progress".to_owned(),
            run_conclusion: None,
            runner_name: Some("runner-rerun".to_owned()),
            runner_id: Some(9001),
            started_at: Some("2020-01-01T00:00:00Z".to_owned()),
            completed_at: None,
        })
        .unwrap_or_else(|_| panic!("record in-progress job state"));
    let service = build_service(settings, journal, true);
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(1)
            .await
            .unwrap_or_else(|_| panic!("in-progress scan")),
        0
    );
    assert!(
        github
            .state
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("requests"))
            .is_empty()
    );
}

pub(crate) async fn newer_attempt_does_not_starve_later_eligible_job() {
    let fixture = RecoveryFixture::new();
    let github = github_fixture().await;
    github.state.scenario.store(9, Ordering::SeqCst);
    github.state.latest_attempt.store(2, Ordering::SeqCst);
    github.state.job_id.store(302, Ordering::SeqCst);
    let settings = build_settings(&fixture, &github.origin);
    let journal = Arc::new(fixture.journal());
    seed_job_and_assignment_for_attempt(
        &journal,
        &settings,
        Some("2020-01-01T00:00:30Z"),
        RerunAssignmentFixture {
            run_id: RUN_ID,
            run_attempt: 1,
            job_id: JOB_ID,
            snapshot_job_id: JOB_ID,
            snapshot_id: "snapshot-rerun-attempt-1",
            allocation_request: "allocation-rerun-attempt-1",
            source_id: "recovery-rerun-attempt-1",
            runner_name: "runner-rerun-1",
        },
    );
    seed_job_and_assignment_for_attempt(
        &journal,
        &settings,
        Some("2020-01-01T00:00:30Z"),
        RerunAssignmentFixture {
            run_id: RUN_ID,
            run_attempt: 2,
            job_id: 302,
            snapshot_job_id: 302,
            snapshot_id: "snapshot-rerun-attempt-2",
            allocation_request: "allocation-rerun-attempt-2",
            source_id: "recovery-rerun-attempt-2",
            runner_name: "runner-rerun-2",
        },
    );
    journal
        .record_job_observation(&JobObservationRecord {
            repository_id: REPOSITORY_ID,
            repository_owner: OWNER.to_owned(),
            repository_name: REPOSITORY.to_owned(),
            run_id: RUN_ID,
            run_attempt: 1,
            job_id: JOB_ID,
            head_sha: SHA.to_owned(),
            status: "completed".to_owned(),
            conclusion: Some("failure".to_owned()),
            run_status: "completed".to_owned(),
            run_conclusion: Some("failure".to_owned()),
            runner_name: Some("runner-rerun-1".to_owned()),
            runner_id: Some(9001),
            started_at: Some("2020-01-01T00:00:00Z".to_owned()),
            completed_at: Some("2020-01-01T00:01:00Z".to_owned()),
        })
        .unwrap_or_else(|_| panic!("persist old terminal attempt"));
    let service = build_service(settings, journal, true);
    let result = service.rerun_completed_interrupted_jobs(2).await;
    assert_eq!(
        result.unwrap_or_else(|error| panic!("scan later eligible attempt: {error:?}")),
        1
    );
    let requests = github
        .state
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("requests"));
    assert!(
        requests
            .iter()
            .any(|request| request.ends_with("/actions/jobs/302/rerun"))
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.ends_with("/actions/jobs/301/rerun"))
    );
}
