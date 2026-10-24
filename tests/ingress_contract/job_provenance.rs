use super::*;

#[tokio::test]
async fn rejection_check_remote_commit_reconciles_after_restart() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(push_run(), job_response(), true)
        .await
        .expect("GitHub API fixture");
    github
        .state
        .fail_check_after_store
        .store(true, Ordering::SeqCst);
    let settings =
        ingress_settings(&directory, &github.origin, "http://127.0.0.1:1/").expect("settings");
    let config = config_with_ingress(settings);
    let journal = directory.journal().expect("journal");
    let service = IngressService::new(config.clone(), journal.clone()).expect("ingress service");
    let body = queued_body(vec!["outrider:v1:101:201:1:BAD"]);
    let (signature, headers) = webhook("workflow_job", "delivery-check-commit", &body);
    let _inserted = service
        .intake(
            "workflow_job",
            "delivery-check-commit",
            &signature,
            headers,
            &body,
        )
        .expect("intake");
    assert_eq!(
        service
            .process_pending(1)
            .await
            .expect("first check attempt"),
        1
    );
    assert_eq!(
        journal.pending_outbox(1).expect("retry outbox")[0].state,
        "retry"
    );
    assert_eq!(github.state.checks.lock().expect("fixture checks").len(), 1);
    drop(service);
    drop(journal);

    let recovered = directory.journal().expect("restart journal");
    let restarted = IngressService::new(config, recovered.clone()).expect("restarted service");
    assert_eq!(
        restarted
            .process_pending(1)
            .await
            .expect("reconcile check after restart"),
        1
    );
    assert!(
        recovered
            .pending_outbox(1)
            .expect("completed outbox")
            .is_empty()
    );
    assert_eq!(github.state.checks.lock().expect("fixture checks").len(), 1);
    assert_eq!(
        github
            .requests()
            .iter()
            .filter(|request| request.method == "POST"
                && request.path == "/repos/base-owner/base-repo/check-runs")
            .count(),
        1
    );
}

#[tokio::test]
async fn retry_limit_one_fairness_and_outbox_poison() {
    let directory = TestDirectory::new().expect("temp directory");
    let settings = ingress_settings(&directory, "http://127.0.0.1:1/", "http://127.0.0.1:1/")
        .expect("settings");
    let journal = directory.journal().expect("journal");
    let service = IngressService::new(config_with_ingress(settings), journal.clone())
        .expect("ingress service");
    for (id, kind) in [
        ("poison", "unknown"),
        ("forward-a", "forward"),
        ("forward-b", "forward"),
    ] {
        journal
            .enqueue_outbox(&OutboxMessage {
                id: id.into(),
                kind: kind.into(),
                destination: CONTROLLER_ID.into(),
                headers_json: r#"{"x-github-event":"workflow_job"}"#.into(),
                body: completed_failure_body(),
                state: "pending".into(),
            })
            .expect("queue outbox message");
    }

    let _processed = service.process_pending(1).await.expect("reject poison row");
    assert_eq!(
        journal.pending_outbox(1).expect("next outbox")[0].id,
        "forward-a"
    );
    assert_eq!(
        journal
            .outbox_state("poison")
            .expect("poison state")
            .as_deref(),
        Some("rejected")
    );
    let _processed = service.process_pending(1).await.expect("retry first relay");
    let pending = journal.pending_outbox(2).expect("fair pending order");
    assert_eq!(pending[0].id, "forward-b");
    assert_eq!(pending[1].id, "forward-a");
    let _processed = service
        .process_pending(1)
        .await
        .expect("retry second relay");
    let pending = journal
        .pending_outbox(2)
        .expect("second fair pending order");
    assert_eq!(pending[0].id, "forward-a");
}

#[tokio::test]
async fn malformed_forward_is_rejected_ahead_of_valid_retry() {
    let directory = TestDirectory::new().expect("temp directory");
    let (service, journal) =
        open_service(&directory, "http://127.0.0.1:1/", "http://127.0.0.1:1/").expect("service");
    for (id, headers) in [
        ("malformed-forward", "not-json"),
        ("valid-forward", r#"{"x-github-event":"workflow_job"}"#),
    ] {
        journal
            .enqueue_outbox(&OutboxMessage {
                id: id.into(),
                kind: "forward".into(),
                destination: CONTROLLER_ID.into(),
                headers_json: headers.into(),
                body: completed_failure_body(),
                state: "pending".into(),
            })
            .expect("queue outbox record");
    }
    let _processed = service
        .process_pending(1)
        .await
        .expect("malformed forward is isolated");
    assert_eq!(
        journal
            .outbox_state("malformed-forward")
            .expect("malformed state")
            .as_deref(),
        Some("rejected")
    );
    assert_eq!(
        journal.pending_outbox(1).expect("valid pending row")[0].id,
        "valid-forward"
    );
}

#[test]
fn schema_two_retry_migration_survives_reopen() {
    let directory = TestDirectory::new().expect("temp directory");
    let path = directory.path.join("journal.sqlite3");
    let connection = rusqlite::Connection::open(&path).expect("legacy journal");
    connection
        .execute_batch(
            "CREATE TABLE schema_version(version INTEGER NOT NULL);
             INSERT INTO schema_version(version) VALUES (2);
             CREATE TABLE webhook_deliveries(
               delivery_id TEXT PRIMARY KEY, repository_id INTEGER NOT NULL, event TEXT NOT NULL,
               headers_json TEXT NOT NULL, body BLOB NOT NULL, state TEXT NOT NULL, detail TEXT,
               created_at INTEGER NOT NULL);
             CREATE TABLE ingress_outbox(
               id TEXT PRIMARY KEY, kind TEXT NOT NULL, destination TEXT NOT NULL,
               headers_json TEXT NOT NULL, body BLOB NOT NULL, state TEXT NOT NULL,
               attempts INTEGER NOT NULL DEFAULT 0, detail TEXT, created_at INTEGER NOT NULL);
             INSERT INTO ingress_outbox(id,kind,destination,headers_json,body,state,created_at)
               VALUES ('legacy','forward','controller','{}',X'01','pending',1);",
        )
        .expect("schema two fixture");
    drop(connection);

    let journal = Journal::open(&path).expect("atomic migration");
    assert_eq!(
        journal.pending_outbox(1).expect("migrated outbox")[0].id,
        "legacy"
    );
    drop(journal);
    let reopened = Journal::open(&path).expect("reopen migrated journal");
    assert_eq!(
        reopened.pending_outbox(1).expect("reopened outbox")[0].id,
        "legacy"
    );
}

#[tokio::test]
async fn canonical_and_extra_label_reject() {
    assert_eq!(
        RoutingRequest::parse(TOKEN)
            .expect("canonical token")
            .format()
            .expect("format"),
        TOKEN
    );
    assert!(RoutingRequest::parse("outrider:v1:101:201:1:build:vcpu=2:vcpu=4").is_err());
    let directory = TestDirectory::new().expect("temp directory");
    let (service, journal) =
        open_service(&directory, "http://127.0.0.1:1/", "http://127.0.0.1:1/").expect("service");
    let body = queued_body(vec![TOKEN, "linux"]);
    let (signature, headers) = webhook("workflow_job", "delivery-3", &body);
    let _inserted = service
        .intake("workflow_job", "delivery-3", &signature, headers, &body)
        .expect("intake");
    let _ = service
        .process_pending(1)
        .await
        .expect("poison-safe worker");
    let outbox = journal.pending_outbox(10).expect("check outbox");
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].kind, "check");
    let check: Value = serde_json::from_slice(&outbox[0].body).expect("rejection body");
    assert_eq!(check["check"]["head_sha"], SHA);
}

#[tokio::test]
async fn immutable_event_commit_matrix() {
    let matrix: Value = serde_json::from_slice(include_bytes!(
        "../acceptance/fixtures/github-event-commit-matrix.json"
    ))
    .expect("fixture matrix");
    let cases = matrix["cases"].as_array().expect("cases array");
    assert_eq!(cases.len(), 8);
    let directory = TestDirectory::new().expect("temp directory");
    let token_file = directory
        .write("matrix-token", b"fixture-token")
        .expect("token file");
    let signature_secret = matrix["webhook_secret"].as_str().expect("fixture secret");
    for case in cases {
        let body = case["wire_body"].as_str().expect("wire body").as_bytes();
        let event: WorkflowJobEvent =
            serde_json::from_slice(body).expect("GitHub workflow-job wire DTO");
        let sha = case["expected_config_sha"]
            .as_str()
            .expect("expected config SHA");
        let expected_token = format!(
            "outrider:v1:{}:{}:{}:build",
            event.repository.id,
            event.workflow_job.run_id,
            event
                .workflow_job
                .run_attempt
                .expect("attempt is in immutable matrix")
        );
        assert_eq!(
            event.workflow_job.labels.first(),
            Some(&expected_token),
            "{}",
            case["name"]
        );
        let signature = case["headers"]["X-Hub-Signature-256"]
            .as_str()
            .expect("signature");
        let mut mac = TestMac::new_from_slice(signature_secret.as_bytes()).expect("matrix secret");
        mac.update(body);
        assert_eq!(
            signature,
            format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
        );

        let server = HttpFixture::start_github(case["run"].clone(), case["job"].clone(), true)
            .await
            .expect("GitHub matrix fixture");
        let client = GitHubClient::from_token_file(
            &server.origin,
            token_file.to_str().expect("token path"),
            std::time::Duration::from_secs(3),
        )
        .expect("GitHub client");
        let allow_forks = case["central_allow_forks"].as_bool().expect("fork policy");
        let expected = case["expected"].as_str().expect("expected outcome");
        if expected.starts_with("reject") {
            assert!(
                client.verify_job(&event, allow_forks).await.is_err(),
                "{}",
                case["name"]
            );
            continue;
        }
        let verified = client
            .verify_job(&event, allow_forks)
            .await
            .expect("exact attempt provenance");
        assert_eq!(verified.run.head_sha, sha, "{}", case["name"]);
        assert_eq!(
            verified.config_repository.id,
            case["run"]["head_repository"]["id"]
                .as_u64()
                .expect("source repository ID"),
            "{}",
            case["name"]
        );
        assert_eq!(
            client
                .repository_config(&verified.base_repository, sha)
                .await
                .expect("exact SHA config"),
            "version: 1\n"
        );
        let expected_content_path =
            format!("/repos/base-owner/base-repo/contents/.outrider.yml?ref={sha}");
        assert!(
            server
                .requests()
                .iter()
                .any(|request| request.path == expected_content_path),
            "{}",
            case["name"]
        );
        if let Some(forbidden) = case["forbidden_substitute_sha"].as_str() {
            assert!(
                !server
                    .requests()
                    .iter()
                    .any(|request| request.path.contains(forbidden)),
                "{}",
                case["name"]
            );
        }
    }
}

#[tokio::test]
async fn provenance_identity_sha_and_private_source_fail_closed() {
    let matrix: Value = serde_json::from_slice(include_bytes!(
        "../acceptance/fixtures/github-event-commit-matrix.json"
    ))
    .expect("fixture matrix");
    let case = &matrix["cases"][0];
    let event: WorkflowJobEvent =
        serde_json::from_str(case["wire_body"].as_str().expect("wire body"))
            .expect("workflow event");
    let directory = TestDirectory::new().expect("temp directory");
    let token_file = directory
        .write("negative-token", b"fixture-token")
        .expect("token file");
    let mut wrong_repository = case["run"].clone();
    wrong_repository["repository"]["id"] = json!(999);
    let server = HttpFixture::start_github(wrong_repository, case["job"].clone(), true)
        .await
        .expect("wrong repository fixture");
    let client = GitHubClient::from_token_file(
        &server.origin,
        token_file.to_str().expect("token path"),
        std::time::Duration::from_secs(3),
    )
    .expect("GitHub client");
    assert!(client.verify_job(&event, false).await.is_err());

    let mut wrong_sha = case["run"].clone();
    wrong_sha["head_sha"] = json!("ffffffffffffffffffffffffffffffffffffffff");
    let server = HttpFixture::start_github(wrong_sha, case["job"].clone(), true)
        .await
        .expect("wrong SHA fixture");
    let client = GitHubClient::from_token_file(
        &server.origin,
        token_file.to_str().expect("token path"),
        std::time::Duration::from_secs(3),
    )
    .expect("GitHub client");
    assert!(client.verify_job(&event, false).await.is_err());

    let server = HttpFixture::start_github(case["run"].clone(), case["job"].clone(), false)
        .await
        .expect("private source fixture");
    let client = GitHubClient::from_token_file(
        &server.origin,
        token_file.to_str().expect("token path"),
        std::time::Duration::from_secs(3),
    )
    .expect("GitHub client");
    let verified = client
        .verify_job(&event, false)
        .await
        .expect("job provenance remains visible");
    assert!(
        client
            .repository_config(
                &verified.base_repository,
                case["expected_config_sha"].as_str().expect("SHA")
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn fork_default_and_central_opt_in() {
    let directory = TestDirectory::new().expect("temp directory");
    let server = HttpFixture::start(FixtureMode::GitHubFork)
        .await
        .expect("GitHub fixture");
    let token_file = directory
        .write("github-token", b"fixture-token")
        .expect("token file");
    let client = GitHubClient::from_token_file(
        &server.origin,
        token_file.to_str().expect("path"),
        std::time::Duration::from_secs(3),
    )
    .expect("GitHub client");
    let event: WorkflowJobEvent =
        serde_json::from_slice(&queued_body(vec![TOKEN])).expect("event DTO");
    assert!(client.verify_job(&event, false).await.is_err());
    assert_eq!(server.requests().len(), 1);
    let verified = client
        .verify_job(&event, true)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "central fork opt-in: {error:?}; requests={:?}",
                server
                    .requests()
                    .iter()
                    .map(|request| request.path.clone())
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(verified.config_repository.id, 102);
    assert_eq!(verified.run.head_sha, SHA);
}

#[test]
fn duplicate_hook_and_terminal_order() {
    let directory = TestDirectory::new().expect("temp directory");
    let journal = directory.journal().expect("journal");
    let delivery = WebhookDelivery {
        delivery_id: "delivery-6".into(),
        repository_id: REPOSITORY_ID,
        event: "workflow_job".into(),
        headers_json: "{}".into(),
        body: b"signed".to_vec(),
        state: "received".into(),
    };
    assert!(journal.persist_delivery(&delivery).expect("first insert"));
    assert!(
        !journal
            .persist_delivery(&delivery)
            .expect("duplicate insert")
    );
    journal
        .record_job_observation(&JobObservationRecord {
            repository_id: REPOSITORY_ID,
            repository_owner: "base-owner".into(),
            repository_name: "base-repo".into(),
            run_id: 201,
            run_attempt: 1,
            job_id: 301,
            head_sha: SHA.into(),
            status: "completed".into(),
            conclusion: Some("failure".into()),
            run_status: "completed".into(),
            run_conclusion: Some("failure".into()),
            runner_name: Some("runner-301".into()),
            runner_id: Some(9001),
            started_at: Some("2026-10-21T00:00:00Z".into()),
            completed_at: Some("2026-10-21T00:01:00Z".into()),
        })
        .expect("completed job");
    journal
        .record_job_observation(&JobObservationRecord {
            repository_id: REPOSITORY_ID,
            repository_owner: "base-owner".into(),
            repository_name: "base-repo".into(),
            run_id: 201,
            run_attempt: 1,
            job_id: 301,
            head_sha: SHA.into(),
            status: "queued".into(),
            conclusion: None,
            run_status: "in_progress".into(),
            run_conclusion: None,
            runner_name: None,
            runner_id: None,
            started_at: None,
            completed_at: None,
        })
        .expect("late queued observation");
    let observation = journal
        .job_observation(REPOSITORY_ID, 201, 1, 301)
        .expect("job query")
        .expect("job");
    assert_eq!(observation.status, "completed");
    assert_eq!(observation.runner_name.as_deref(), Some("runner-301"));
    journal
        .mark_run_terminal(REPOSITORY_ID, 201, 1)
        .expect("terminal run");
    assert!(
        journal
            .run_is_terminal(REPOSITORY_ID, 201, 1)
            .expect("terminal check")
    );
}

#[tokio::test]
async fn poison_no_head_of_line() {
    let directory = TestDirectory::new().expect("temp directory");
    let (service, journal) =
        open_service(&directory, "http://127.0.0.1:1/", "http://127.0.0.1:1/").expect("service");
    let poison = br#"{"action":"queued","repository":{"id":101},"workflow_job":{"id":"bad"}}"#;
    let (signature, headers) = webhook("workflow_job", "delivery-poison", poison);
    let _first = service
        .intake(
            "workflow_job",
            "delivery-poison",
            &signature,
            headers,
            poison,
        )
        .expect("signed poison");
    let ordinary = br#"{"repository":{"id":101,"name":"base-repo","full_name":"base-owner/base-repo","owner":{"login":"base-owner"}},"action":"created"}"#;
    let (signature, headers) = webhook("repository", "delivery-valid", ordinary);
    let _second = service
        .intake(
            "repository",
            "delivery-valid",
            &signature,
            headers,
            ordinary,
        )
        .expect("valid hook");
    let _count = service.process_pending(10).await.expect("process both");
    assert_eq!(
        journal
            .delivery_state("delivery-poison")
            .expect("state")
            .as_deref(),
        Some("rejected")
    );
    assert_eq!(
        journal
            .delivery_state("delivery-valid")
            .expect("state")
            .as_deref(),
        Some("queued")
    );
    assert!(
        journal
            .pending_outbox(10)
            .expect("forward outbox")
            .is_empty()
    );
}
