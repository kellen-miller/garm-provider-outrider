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
    let service = IngressService::new(full_ingress_config(settings), journal.clone())
        .expect("production ingress service");

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
