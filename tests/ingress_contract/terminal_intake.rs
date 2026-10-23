use super::*;

#[tokio::test]
async fn unrelated_jobs_ack_without_dependency_work() {
    let directory = TestDirectory::new().expect("temp directory");
    let (service, journal) =
        open_service(&directory, "http://127.0.0.1:1/", "http://127.0.0.1:1/").expect("service");
    for status in ["queued", "completed"] {
        let mut event: Value =
            serde_json::from_slice(&queued_body(vec!["ubuntu-24.04"])).expect("event");
        event["action"] = json!(status);
        event["workflow_job"]["status"] = json!(status);
        let body = serde_json::to_vec(&event).expect("event bytes");
        let (signature, headers) = webhook("workflow_job", status, &body);
        service
            .intake("workflow_job", status, &signature, headers, &body)
            .expect("authenticated hosted job");
    }

    assert_eq!(service.process_pending(2).await.expect("process inbox"), 2);
    assert!(journal.pending_outbox(10).expect("outbox").is_empty());
    assert!(
        journal
            .pool_snapshot(REPOSITORY_ID, TOKEN)
            .expect("snapshot")
            .is_none()
    );
}

#[tokio::test]
async fn terminal_intake_atomically_suppresses_late_queued_admission() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(push_run(), job_response(), true)
        .await
        .expect("GitHub API fixture");
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM API fixture");
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let mut config = full_ingress_config(settings);
    config.admission = Some(outrider::config::AdmissionSettings {
        state_file: directory.path.join("missing-admission.json"),
        max_window_seconds: 1800,
    });
    let service = IngressService::new(config, journal.clone()).expect("production ingress service");

    let queued = queued_body(vec![TOKEN]);
    let (queued_signature, queued_headers) = webhook("workflow_job", "delivery-late", &queued);
    service
        .intake(
            "workflow_job",
            "delivery-late",
            &queued_signature,
            queued_headers,
            &queued,
        )
        .expect("queued intake");
    assert_eq!(
        service
            .process_pending(1)
            .await
            .expect("closed queued delivery"),
        0
    );
    assert_eq!(
        journal
            .delivery_state("delivery-late")
            .expect("delivery state")
            .as_deref(),
        Some("retry")
    );
    assert!(
        garm.requests()
            .iter()
            .all(|request| request.method != "POST" || !request.path.ends_with("/pools"))
    );
    assert!(
        github
            .requests()
            .iter()
            .all(|request| !request.path.ends_with("/check-runs"))
    );

    let completed = json!({
        "action":"completed",
        "repository":{"id":REPOSITORY_ID,"name":"base-repo","full_name":"base-owner/base-repo","owner":{"login":"base-owner"}},
        "workflow_run":{"id":201,"run_attempt":1,"head_sha":SHA}
    });
    let completed = serde_json::to_vec(&completed).expect("workflow run event");
    let (completed_signature, completed_headers) =
        webhook("workflow_run", "delivery-complete", &completed);
    service
        .intake(
            "workflow_run",
            "delivery-complete",
            &completed_signature,
            completed_headers,
            &completed,
        )
        .expect("terminal intake persists with event");
    assert!(
        journal
            .run_is_terminal(REPOSITORY_ID, 201, 1)
            .expect("terminal marker before worker")
    );

    assert_eq!(
        service
            .process_pending(1)
            .await
            .expect("late queued delivery"),
        1
    );
    assert!(
        journal
            .pool_snapshot(REPOSITORY_ID, TOKEN)
            .expect("snapshot lookup")
            .is_none()
    );
    assert!(
        garm.requests()
            .iter()
            .all(|request| request.method != "POST" || !request.path.ends_with("/pools"))
    );

    assert_eq!(
        service
            .process_pending(1)
            .await
            .expect("local completion handling"),
        1
    );
    assert!(garm.requests().is_empty());
}

#[tokio::test]
async fn closed_admission_relays_owned_lifecycle_and_rejects_unrelated_outbox() {
    let directory = TestDirectory::new().expect("temp directory");
    let github = HttpFixture::start_github(push_run(), job_response(), true)
        .await
        .expect("GitHub fixture");
    let journal = directory.journal().expect("journal");
    let garm = HttpFixture::start_garm(journal.clone())
        .await
        .expect("GARM fixture");
    let settings = ingress_settings(&directory, &github.origin, &garm.origin).expect("settings");
    let mut config = full_ingress_config(settings);
    config.admission = Some(outrider::config::AdmissionSettings {
        state_file: directory.path.join("missing-admission.json"),
        max_window_seconds: 1800,
    });
    let service = IngressService::new(config, journal.clone()).expect("ingress");
    struct PendingForward {
        id: &'static str,
        body: Vec<u8>,
    }

    let forwards = [
        PendingForward {
            id: "window-queued",
            body: queued_body(vec![TOKEN]),
        },
        PendingForward {
            id: "window-progress",
            body: in_progress_body(),
        },
        PendingForward {
            id: "window-completed",
            body: completed_failure_body(),
        },
        PendingForward {
            id: "unrelated-queued",
            body: queued_body(vec!["self-hosted"]),
        },
    ];
    for forward in &forwards {
        let (_, headers) = webhook("workflow_job", forward.id, &forward.body);
        let headers = headers
            .into_iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value))
            .collect::<BTreeMap<_, _>>();
        journal
            .enqueue_outbox(&OutboxMessage {
                id: forward.id.into(),
                kind: "forward".into(),
                destination: CONTROLLER_ID.into(),
                headers_json: serde_json::to_string(&headers).expect("forward headers"),
                body: forward.body.clone(),
                state: "pending".into(),
            })
            .expect("persist pending forward");
    }

    assert_eq!(
        service
            .process_pending(4)
            .await
            .expect("closed outbox processing"),
        3
    );
    let pending = journal.pending_outbox(4).expect("pending forwards");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "window-queued");
    let requests = garm.requests();
    assert!(
        !requests
            .iter()
            .any(|request| request.body == forwards[0].body)
    );
    for forward in &forwards[1..3] {
        assert!(requests.iter().any(|request| request.body == forward.body));
    }

    assert!(
        requests
            .iter()
            .all(|request| request.body != forwards[3].body)
    );
    assert_eq!(
        journal
            .outbox_state("unrelated-queued")
            .expect("outside scope state")
            .as_deref(),
        Some("rejected")
    );
}
