use super::*;

#[tokio::test]
async fn default_off_and_caps() {
    rerun_http::default_off_and_caps().await;
}

#[tokio::test]
async fn per_job_endpoint_after_completion() {
    rerun_http::per_job_endpoint_after_completion().await;
}

#[tokio::test]
async fn ambiguous_rerun_observation() {
    rerun_http::ambiguous_rerun_observation().await;
}

#[tokio::test]
async fn forbidden_rerun_is_terminal_after_restart() {
    rerun_http::forbidden_rerun_is_terminal_after_restart().await;
}

#[tokio::test]
async fn lost_rerun_response_keeps_intent_after_restart() {
    rerun_http::lost_rerun_response_keeps_intent_after_restart().await;
}

#[tokio::test]
async fn rerun_denial_observations() {
    rerun_http::rerun_denial_observations().await;
}

#[tokio::test]
async fn rerun_waits_for_job_completion() {
    rerun_http::rerun_waits_for_job_completion().await;
}

#[tokio::test]
async fn newer_attempt_does_not_starve_later_eligible_job() {
    rerun_http::newer_attempt_does_not_starve_later_eligible_job().await;
}

#[tokio::test]
async fn recovery_iteration_runs_bounded_retention_and_independent_work() {
    rerun_http::retention_runs_in_recovery_iteration_with_independent_inbox_work().await;
}

#[test]
fn rerun_admin_settings_share_normalization_boundary() {
    let central = outrider::config::CentralConfig::from_toml(include_str!(
        "../../examples/config-minimal.toml"
    ))
    .and_then(|source| source.load_native_configuration(&std::collections::BTreeMap::new()))
    .unwrap_or_else(|_| panic!("load valid central configuration"));
    let repository = outrider::config::ConfigPatch {
        reruns: Some(outrider::config::RerunSettings {
            enabled: true,
            max_per_run: 2,
            actions_write_permission: true,
        }),
        ..outrider::config::ConfigPatch::default()
    };
    assert!(
        central
            .normalize_profile(Some(&repository), "linux-large", None)
            .is_err()
    );

    let mut invalid_caps = central;
    invalid_caps.reruns = Some(outrider::config::RerunSettings {
        enabled: false,
        max_per_run: 0,
        actions_write_permission: false,
    });
    assert!(
        invalid_caps
            .normalize_profile(None, "linux-large", None)
            .is_err()
    );
}

#[test]
fn rerun_intent_rejects_unknown_states() {
    let fixture = RecoveryFixture::new();
    let journal = fixture.journal();
    journal
        .reserve_job_rerun(&outrider::journal::JobRerunReservation {
            repository_id: 7,
            owner: "example".to_owned(),
            repository: "outrider".to_owned(),
            github_api_origin: "http://github.fixture/".to_owned(),
            run_id: 201,
            run_attempt: 1,
            job_id: 301,
            max_per_run: 1,
        })
        .unwrap_or_else(|_| panic!("reserve intent for state validation"));
    assert!(
        journal
            .set_job_rerun_state(7, 201, 1, 301, "unknown")
            .is_err()
    );
    assert!(
        journal
            .set_job_rerun_state(7, 201, 1, 301, "denied")
            .is_ok()
    );
    assert!(
        journal
            .pending_job_reruns(1)
            .unwrap_or_else(|_| panic!("read denied terminal intent"))
            .is_empty()
    );
}

#[test]
fn rerun_terminal_observations_enrich_unknowns_and_reject_conflicts() {
    let fixture = RecoveryFixture::new();
    let journal = fixture.journal();
    let mut legacy = rerun_observation();
    legacy.conclusion = None;
    legacy.runner_name = None;
    legacy.runner_id = None;
    legacy.started_at = None;
    legacy.completed_at = None;
    legacy.run_status = "unknown".to_owned();
    legacy.run_conclusion = None;
    legacy.repository_owner.clear();
    legacy.repository_name.clear();
    journal
        .record_job_observation(&legacy)
        .unwrap_or_else(|_| panic!("persist legacy terminal observation"));
    let mut enriched = rerun_observation();
    enriched.run_status = "completed".to_owned();
    enriched.run_conclusion = Some("cancelled".to_owned());
    enriched.repository_owner = "owner".to_owned();
    enriched.repository_name = "repository".to_owned();
    journal
        .record_job_observation(&enriched)
        .unwrap_or_else(|_| panic!("enrich missing terminal facts"));
    let mut stale = rerun_observation();
    stale.status = "queued".to_owned();
    stale.conclusion = None;
    stale.runner_name = None;
    stale.runner_id = None;
    stale.started_at = None;
    stale.completed_at = None;
    stale.run_status = "in_progress".to_owned();
    stale.run_conclusion = None;
    stale.repository_owner.clear();
    stale.repository_name.clear();
    journal
        .record_job_observation(&stale)
        .unwrap_or_else(|_| panic!("ignore stale incomplete observation"));
    let stored = journal
        .job_observation(7, 201, 1, 301)
        .unwrap_or_else(|_| panic!("read terminal facts"))
        .unwrap_or_else(|| panic!("terminal record"));
    assert_eq!(stored.conclusion.as_deref(), Some("failure"));
    assert_eq!(stored.runner_id, Some(9));
    assert_eq!(stored.repository_owner, "owner");
    assert_eq!(stored.run_conclusion.as_deref(), Some("cancelled"));
    let mut conflicting = enriched;
    conflicting.conclusion = Some("success".to_owned());
    assert!(journal.record_job_observation(&conflicting).is_err());
}

#[test]
fn in_progress_job_enriches_runner_assignment_before_completion() {
    let fixture = RecoveryFixture::new();
    let journal = fixture.journal();
    let mut queued = rerun_observation();
    queued.status = "queued".to_owned();
    queued.conclusion = None;
    queued.runner_name = None;
    queued.runner_id = None;
    queued.started_at = None;
    queued.completed_at = None;
    queued.run_status = "queued".to_owned();
    queued.run_conclusion = None;
    journal
        .record_job_observation(&queued)
        .unwrap_or_else(|_| panic!("persist queued observation"));

    let mut assigned = queued.clone();
    assigned.status = "in_progress".to_owned();
    assigned.runner_name = Some("runner-live".to_owned());
    assigned.runner_id = Some(88);
    assigned.started_at = Some("2026-10-21T12:00:00Z".to_owned());
    assigned.run_status = "in_progress".to_owned();
    journal
        .record_job_observation(&assigned)
        .unwrap_or_else(|_| panic!("persist in-progress assignment"));
    let stored = journal
        .job_observation(7, 201, 1, 301)
        .unwrap_or_else(|_| panic!("read in-progress assignment"))
        .unwrap_or_else(|| panic!("in-progress observation"));
    assert_eq!(stored.status, "in_progress");
    assert_eq!(stored.runner_name.as_deref(), Some("runner-live"));
    assert_eq!(stored.runner_id, Some(88));
    assert_eq!(stored.started_at.as_deref(), Some("2026-10-21T12:00:00Z"));

    let mut completed = assigned.clone();
    completed.status = "completed".to_owned();
    completed.conclusion = Some("failure".to_owned());
    completed.completed_at = Some("2026-10-21T12:10:00Z".to_owned());
    completed.run_status = "completed".to_owned();
    journal
        .record_job_observation(&completed)
        .unwrap_or_else(|_| panic!("persist completed assignment"));
    let mut stale = assigned;
    stale.runner_name = Some("runner-replaced".to_owned());
    stale.runner_id = Some(99);
    stale.started_at = Some("2026-10-21T11:00:00Z".to_owned());
    journal
        .record_job_observation(&stale)
        .unwrap_or_else(|_| panic!("ignore stale in-progress assignment"));
    let stored = journal
        .job_observation(7, 201, 1, 301)
        .unwrap_or_else(|_| panic!("read terminal assignment"))
        .unwrap_or_else(|| panic!("terminal observation"));
    assert_eq!(stored.runner_name.as_deref(), Some("runner-live"));
    assert_eq!(stored.runner_id, Some(88));
    assert_eq!(stored.started_at.as_deref(), Some("2026-10-21T12:00:00Z"));
    assert_eq!(stored.completed_at.as_deref(), Some("2026-10-21T12:10:00Z"));
    assert_eq!(stored.conclusion.as_deref(), Some("failure"));
}

pub(super) fn rerun_observation() -> outrider::journal::JobObservationRecord {
    outrider::journal::JobObservationRecord {
        repository_id: 7,
        repository_owner: String::new(),
        repository_name: String::new(),
        run_id: 201,
        run_attempt: 1,
        job_id: 301,
        head_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        status: "completed".to_owned(),
        conclusion: Some("failure".to_owned()),
        run_status: "completed".to_owned(),
        run_conclusion: None,
        runner_name: Some("runner".to_owned()),
        runner_id: Some(9),
        started_at: Some("2020-01-01T00:00:00Z".to_owned()),
        completed_at: Some("2020-01-01T00:01:00Z".to_owned()),
    }
}
