use super::*;
#[path = "../fixtures/admission_state.rs"]
mod admission_state;
use admission_state::AdmissionState;

#[tokio::test]
async fn closed_restart_blocks_new_allocation_but_preserves_existing_reads_and_delete() {
    let mut harness = AllocationHarness::new(AllocationHarnessOptions::default()).await;
    let state = AdmissionState::new();
    harness.current_config.admission = Some(state.settings.clone());
    let service = rebuild_allocation_service(
        &harness._fixture,
        Arc::clone(&harness.journal),
        &harness.aws_endpoint,
        &harness.old_config,
        &harness.current_config,
        "record",
    )
    .await;
    assert!(matches!(
        service.handle(harness.request.clone()).await,
        Err(AllocationError::AdmissionClosed)
    ));
    assert!(!harness.record_path.exists());
    assert!(
        harness
            .journal
            .allocation("controller-1", "runner-old-image")
            .expect("allocation lookup")
            .is_none()
    );

    let now = AdmissionState::now();
    state.write(now - 1, now + 600);
    let created = service
        .handle(harness.request.clone())
        .await
        .expect("open create");
    assert!(created.is_some());
    state.write(now - 1800, now);
    assert_eq!(
        service
            .handle(harness.request.clone())
            .await
            .expect("idempotent created result"),
        created
    );
    let mut delayed = harness.request.clone();
    delayed.bootstrap.as_mut().expect("bootstrap").name = "delayed-runner".into();
    assert!(matches!(
        service.handle(delayed.clone()).await,
        Err(AllocationError::AdmissionClosed)
    ));
    let mut list = harness.request.clone();
    list.command = "ListInstances".into();
    list.bootstrap = None;
    list.instance_id = None;
    list.pool_extra_specs = None;
    assert!(service.handle(list).await.is_ok());
    assert!(
        service
            .handle(harness.lifecycle_request("GetInstance", "runner-old-image"))
            .await
            .is_ok()
    );
    assert!(
        service
            .handle(harness.lifecycle_request("DeleteInstance", "runner-old-image"))
            .await
            .is_ok()
    );
    assert_eq!(harness.allocation().state, "deleted");
    drop(service);

    fs::remove_file(&state.settings.state_file).expect("remove state before restart");
    let reopened = rebuild_allocation_service(
        &harness._fixture,
        Arc::clone(&harness.journal),
        &harness.aws_endpoint,
        &harness.old_config,
        &harness.current_config,
        "record",
    )
    .await;
    assert!(matches!(
        reopened.handle(delayed).await,
        Err(AllocationError::AdmissionClosed)
    ));
}
