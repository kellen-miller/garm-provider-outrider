use super::*;

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
        service.process_pending(1).await.expect("completion relay"),
        2
    );
    assert!(garm.requests().iter().any(|request| request.path
        == "/webhooks/00000000-0000-0000-0000-000000000901"
        && request.body == completed));
}

#[tokio::test]
async fn closed_admission_holds_queued_outbox_but_relays_progress_and_unrelated_jobs() {
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
    for forward in forwards.iter().skip(1) {
        assert!(requests.iter().any(|request| request.body == forward.body));
    }
}
