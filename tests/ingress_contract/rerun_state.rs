use super::*;

#[tokio::test]
async fn common_run_triggers_use_authenticated_run_sha() {
    let directory = TestDirectory::new().expect("temp directory");
    let token_file = directory
        .write("github-trigger-token", b"fixture-token")
        .expect("token file");
    for event_name in [
        "workflow_dispatch",
        "schedule",
        "release",
        "repository_dispatch",
    ] {
        let mut run = push_run();
        run["event"] = json!(event_name);
        let server = HttpFixture::start_github(run, job_response(), true)
            .await
            .expect("GitHub API fixture");
        let client = GitHubClient::from_token_file(
            &server.origin,
            token_file.to_str().expect("token path"),
            Duration::from_secs(3),
        )
        .expect("GitHub client");
        let event: WorkflowJobEvent =
            serde_json::from_slice(&queued_body(vec![TOKEN])).expect("workflow job event");
        let verified = client
            .verify_job(&event, false)
            .await
            .expect("verified trigger provenance");
        assert_eq!(verified.config_repository.id, REPOSITORY_ID);
        assert_eq!(verified.run.head_sha, SHA);
        client
            .repository_config(&verified.base_repository, &verified.run.head_sha)
            .await
            .expect("config at exact run SHA");
        assert!(server.requests().iter().any(|request| {
            request.path == format!("/repos/base-owner/base-repo/contents/.outrider.toml?ref={SHA}")
        }));

        let mut inconsistent_run = push_run();
        inconsistent_run["event"] = json!(event_name);
        inconsistent_run["head_repository"]["id"] = json!(999);
        let server = HttpFixture::start_github(inconsistent_run, job_response(), true)
            .await
            .expect("inconsistent trigger fixture");
        let client = GitHubClient::from_token_file(
            &server.origin,
            token_file.to_str().expect("token path"),
            Duration::from_secs(3),
        )
        .expect("GitHub client");
        assert!(client.verify_job(&event, true).await.is_err());
    }
}

#[tokio::test]
async fn default_off_and_caps() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(failed_run(), failed_assigned_job(), true)
        .await
        .expect("GitHub fixture");
    let journal = directory.journal().expect("journal");
    let settings =
        ingress_settings(&directory, &github.origin, "http://127.0.0.1:1/").expect("settings");
    seed_interrupted_assignment(&journal, &settings);
    let service = IngressService::new(config_with_ingress(settings), journal.clone())
        .expect("default-off ingress");
    let body = completed_failure_body();
    let (signature, headers) = webhook("workflow_job", "rerun-default-off", &body);
    service
        .intake(
            "workflow_job",
            "rerun-default-off",
            &signature,
            headers,
            &body,
        )
        .expect("authenticated completion");
    service
        .process_pending(10)
        .await
        .expect("process completion");
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(10)
            .await
            .expect("default-off pass"),
        0
    );
    assert!(
        github
            .requests()
            .iter()
            .all(|request| !request.path.ends_with("/rerun"))
    );
    assert!(
        journal
            .pending_job_reruns(10)
            .expect("no rerun intent")
            .is_empty()
    );
    assert!(
        journal
            .reserve_job_rerun(&rerun_reservation("http://github.fixture/", 1, 301, 1))
            .expect("first bounded reservation")
    );
    assert!(
        !journal
            .reserve_job_rerun(&rerun_reservation("http://github.fixture/", 1, 301, 1))
            .expect("same exact job fence")
    );
    assert!(
        journal
            .reserve_job_rerun(&rerun_reservation("http://github.fixture/", 1, 302, 2))
            .expect("second stable run slot")
    );
    assert!(
        !journal
            .reserve_job_rerun(&rerun_reservation("http://github.fixture/", 2, 303, 2))
            .expect("cap remains stable across attempts")
    );
}

pub(super) fn rerun_reservation(
    github_api_origin: &str,
    run_attempt: u32,
    job_id: u64,
    max_per_run: u32,
) -> JobRerunReservation {
    JobRerunReservation {
        repository_id: REPOSITORY_ID,
        owner: "base-owner".to_owned(),
        repository: "base-repo".to_owned(),
        github_api_origin: github_api_origin.to_owned(),
        run_id: 201,
        run_attempt,
        job_id,
        max_per_run,
    }
}

#[tokio::test]
async fn per_job_endpoint_after_completion() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(failed_run(), failed_assigned_job(), true)
        .await
        .expect("GitHub fixture");
    let journal = directory.journal().expect("journal");
    let settings =
        ingress_settings(&directory, &github.origin, "http://127.0.0.1:1/").expect("settings");
    seed_interrupted_assignment(&journal, &settings);
    let service = IngressService::new(enabled_rerun_config(settings), journal.clone())
        .expect("rerun-enabled ingress");
    let body = completed_failure_body();
    let (signature, headers) = webhook("workflow_job", "rerun-completed", &body);
    service
        .intake(
            "workflow_job",
            "rerun-completed",
            &signature,
            headers,
            &body,
        )
        .expect("authenticated completion");
    service
        .process_pending(10)
        .await
        .expect("process completion");
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(10)
            .await
            .expect("run bounded rerun pass"),
        1
    );
    let requests = github.requests();
    let rerun = requests
        .iter()
        .find(|request| request.method == "POST" && request.path.ends_with("/rerun"))
        .expect("exact-job rerun request");
    assert_eq!(
        rerun.path,
        "/repos/base-owner/base-repo/actions/jobs/301/rerun"
    );
    assert_eq!(rerun.authorization.as_deref(), Some("Bearer github-token"));
    assert!(rerun.body.is_empty());
    assert_eq!(
        journal.pending_job_reruns(10).expect("acknowledged intent")[0].state,
        "acknowledged"
    );
}

#[tokio::test]
async fn queued_empty_runner_placeholders_enrich_through_completion_and_rerun() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(failed_run(), queued_placeholder_job(), true)
        .await
        .expect("GitHub API fixture");
    let journal = directory.journal().expect("journal");
    let settings =
        ingress_settings(&directory, &github.origin, "http://127.0.0.1:1/").expect("settings");
    seed_interrupted_assignment(&journal, &settings);

    let queued: WorkflowJobEvent =
        serde_json::from_slice(&queued_body(vec![TOKEN])).expect("queued event");
    let client = GitHubClient::from_token_file(
        &github.origin,
        &settings.github_token_file,
        std::time::Duration::from_secs(3),
    )
    .expect("GitHub client");
    let verified = client
        .verify_job(&queued, false)
        .await
        .expect("authenticated queued assignment observation");
    journal
        .record_job_observation(&outrider::journal::JobObservationRecord {
            repository_id: verified.observation.repository_id,
            repository_owner: verified.observation.repository_owner,
            repository_name: verified.observation.repository_name,
            run_id: verified.observation.run_id,
            run_attempt: verified.observation.run_attempt,
            job_id: verified.observation.job_id,
            head_sha: verified.observation.head_sha,
            status: verified.observation.status,
            conclusion: verified.observation.conclusion,
            run_status: verified.observation.run_status,
            run_conclusion: verified.observation.run_conclusion,
            runner_name: verified.observation.runner_name,
            runner_id: verified.observation.runner_id,
            started_at: verified.observation.started_at,
            completed_at: verified.observation.completed_at,
        })
        .expect("persist authenticated queued placeholders");
    let normalized = journal
        .job_observation(REPOSITORY_ID, 201, 1, 301)
        .expect("read normalized queued observation")
        .expect("queued observation exists");
    assert_eq!(normalized.runner_name, None);
    assert_eq!(normalized.runner_id, None);
    drop(journal);

    let database = rusqlite::Connection::open(directory.path.join("journal.sqlite3"))
        .expect("open legacy placeholder journal");
    database
        .execute(
            "UPDATE job_observations SET runner_name='', runner_id=0 WHERE repository_id=?1 AND run_id=201 AND run_attempt=1 AND job_id=301",
            [i64::try_from(REPOSITORY_ID).expect("repository ID fits SQLite")],
        )
        .expect("persist pre-normalization placeholders");
    drop(database);

    let journal = directory.journal().expect("reopened journal");
    let service = IngressService::new(enabled_rerun_config(settings.clone()), journal.clone())
        .expect("rerun-enabled ingress");
    github.set_github_job(in_progress_assigned_job());
    let in_progress = in_progress_body();
    let (progress_signature, progress_headers) =
        webhook("workflow_job", "placeholder-in-progress", &in_progress);
    service
        .intake(
            "workflow_job",
            "placeholder-in-progress",
            &progress_signature,
            progress_headers,
            &in_progress,
        )
        .expect("signed in-progress intake");
    service
        .process_pending(10)
        .await
        .expect("process in-progress assignment");
    let assigned = journal
        .job_observation(REPOSITORY_ID, 201, 1, 301)
        .expect("read in-progress observation")
        .expect("in-progress observation exists");
    assert_eq!(assigned.runner_name.as_deref(), Some("runner-301"));
    assert_eq!(assigned.runner_id, Some(9001));

    github.set_github_job(failed_assigned_job());
    let completed = completed_failure_body();
    let (completion_signature, completion_headers) =
        webhook("workflow_job", "placeholder-completed", &completed);
    service
        .intake(
            "workflow_job",
            "placeholder-completed",
            &completion_signature,
            completion_headers,
            &completed,
        )
        .expect("signed completed intake");
    service
        .process_pending(10)
        .await
        .expect("process completed assignment");
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(10)
            .await
            .expect("authenticated bounded rerun eligibility"),
        1
    );
    let rerun = github
        .requests()
        .into_iter()
        .find(|request| request.method == "POST" && request.path.ends_with("/rerun"))
        .expect("exact-job rerun post");
    assert_eq!(
        rerun.path,
        "/repos/base-owner/base-repo/actions/jobs/301/rerun"
    );
    assert_eq!(rerun.authorization.as_deref(), Some("Bearer github-token"));
}

#[tokio::test]
async fn ambiguous_rerun_observation() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(failed_run(), failed_assigned_job(), true)
        .await
        .expect("GitHub fixture");
    github.state.rerun_ack_status.store(500, Ordering::SeqCst);
    let journal = directory.journal().expect("journal");
    let settings =
        ingress_settings(&directory, &github.origin, "http://127.0.0.1:1/").expect("settings");
    seed_interrupted_assignment(&journal, &settings);
    let config = enabled_rerun_config(settings);
    let service = IngressService::new(config.clone(), journal.clone()).expect("rerun ingress");
    let body = completed_failure_body();
    let (signature, headers) = webhook("workflow_job", "rerun-ambiguous", &body);
    service
        .intake(
            "workflow_job",
            "rerun-ambiguous",
            &signature,
            headers,
            &body,
        )
        .expect("authenticated completion");
    service
        .process_pending(10)
        .await
        .expect("retain ambiguous intent");
    assert!(service.rerun_completed_interrupted_jobs(10).await.is_err());
    assert_eq!(
        journal.pending_job_reruns(10).expect("ambiguous intent")[0].state,
        "intent"
    );
    drop(service);
    drop(journal);
    let journal = directory.journal().expect("reopened journal");
    github.state.latest_run_attempt.store(2, Ordering::SeqCst);
    let service = IngressService::new(config, journal.clone()).expect("reopened ingress");
    service
        .process_pending(10)
        .await
        .expect("observe later attempt without resubmitting");
    assert_eq!(
        github
            .requests()
            .iter()
            .filter(|request| request.method == "POST" && request.path.ends_with("/rerun"))
            .count(),
        1
    );
    assert!(
        journal
            .pending_job_reruns(10)
            .expect("observed intent is terminal")
            .is_empty()
    );
}

#[tokio::test]
async fn forbidden_rerun_is_a_durable_denial() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(failed_run(), failed_assigned_job(), true)
        .await
        .expect("GitHub fixture");
    github.state.rerun_ack_status.store(403, Ordering::SeqCst);
    let journal = directory.journal().expect("journal");
    let settings =
        ingress_settings(&directory, &github.origin, "http://127.0.0.1:1/").expect("settings");
    seed_interrupted_assignment(&journal, &settings);
    let config = enabled_rerun_config(settings);
    let service = IngressService::new(config.clone(), journal.clone()).expect("rerun ingress");
    let body = completed_failure_body();
    let (signature, headers) = webhook("workflow_job", "rerun-forbidden", &body);
    service
        .intake(
            "workflow_job",
            "rerun-forbidden",
            &signature,
            headers,
            &body,
        )
        .expect("authenticated completion");
    service
        .process_pending(10)
        .await
        .expect("process completion observation");
    assert_eq!(
        service
            .rerun_completed_interrupted_jobs(10)
            .await
            .expect("persist definitive denial"),
        0
    );
    assert!(
        journal
            .pending_job_reruns(10)
            .expect("denied intent is terminal")
            .is_empty()
    );
    assert_eq!(
        github
            .requests()
            .iter()
            .filter(|request| request.method == "POST" && request.path.ends_with("/rerun"))
            .count(),
        1
    );
    assert!(
        !journal
            .reserve_job_rerun(&rerun_reservation(&github.origin, 1, 301, 1))
            .expect("denial retains dedup and cap slot")
    );
}
