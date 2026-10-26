use std::{
    fs, io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use http_body_util::BodyExt;
use hyper::{Request, Response, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use outrider::{
    cloud::PlacementIdentity,
    config::{ConfigPatch, IngressRepository, IngressSettings, RerunSettings},
    github::{IngressService, OwnerRef, RepositoryRef},
    journal::{
        ChildAttemptRecord, InterruptionRecord, JobObservationRecord, JobRerunReservation,
        PoolSnapshot, WebhookDelivery,
    },
    recovery::RecoveryService,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use super::{RecoveryFixture, allocation_harness};

pub(crate) const REPOSITORY_ID: u64 = 7;
pub(crate) const RUN_ID: u64 = 201;
pub(crate) const JOB_ID: u64 = 301;
pub(crate) const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
pub(crate) const OWNER: &str = "example";
pub(crate) const REPOSITORY: &str = "outrider";

pub(crate) struct RerunAssignmentFixture<'a> {
    pub(crate) run_id: u64,
    pub(crate) run_attempt: u32,
    pub(crate) job_id: u64,
    pub(crate) snapshot_job_id: u64,
    pub(crate) snapshot_id: &'a str,
    pub(crate) allocation_request: &'a str,
    pub(crate) source_id: &'a str,
    pub(crate) runner_name: &'a str,
}

pub(crate) fn rerun_runner_name(job_id: u64) -> String {
    if job_id == JOB_ID {
        "runner-rerun".to_owned()
    } else {
        format!("runner-rerun-{job_id}")
    }
}

pub(crate) struct GitHubFixture {
    pub(crate) origin: String,
    pub(crate) state: Arc<RerunHttpState>,
    pub(crate) task: tokio::task::JoinHandle<io::Result<()>>,
}

pub(crate) struct RerunHttpState {
    pub(crate) latest_attempt: AtomicU64,
    pub(crate) scenario: AtomicU64,
    pub(crate) job_id: AtomicU64,
    pub(crate) rerun_status: AtomicU64,
    pub(crate) drop_rerun_response: AtomicBool,
    pub(crate) requests: Mutex<Vec<String>>,
}

impl Drop for GitHubFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) async fn github_fixture() -> GitHubFixture {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|_| panic!("bind local GitHub fixture"));
    let origin = format!(
        "http://{}/",
        listener
            .local_addr()
            .unwrap_or_else(|_| panic!("fixture address"))
    );
    let state = Arc::new(RerunHttpState {
        latest_attempt: AtomicU64::new(1),
        scenario: AtomicU64::new(0),
        job_id: AtomicU64::new(JOB_ID),
        rerun_status: AtomicU64::new(201),
        drop_rerun_response: AtomicBool::new(false),
        requests: Mutex::new(Vec::new()),
    });
    let server_state = state.clone();
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await?;
            serve_rerun_connection(stream, server_state.clone());
        }
    });
    GitHubFixture {
        origin,
        state,
        task,
    }
}

pub(crate) fn serve_rerun_connection(stream: tokio::net::TcpStream, state: Arc<RerunHttpState>) {
    tokio::spawn(async move {
        let service = service_fn(move |request| rerun_request(request, state.clone()));
        let _result = http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .await;
    });
}

pub(crate) async fn rerun_request(
    request: Request<Incoming>,
    state: Arc<RerunHttpState>,
) -> Result<Response<http_body_util::Full<bytes::Bytes>>, Box<dyn std::error::Error + Send + Sync>>
{
    let method = request.method().to_string();
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some("Bearer github-fixture-token");
    let path = request.uri().path_and_query().map_or_else(
        || request.uri().path().to_owned(),
        |value| value.as_str().to_owned(),
    );
    let _body = request.into_body().collect().await?.to_bytes();
    if let Ok(mut records) = state.requests.lock() {
        records.push(format!("{method} {path}"));
    }
    let run = |attempt, current| {
        let scenario = state.scenario.load(Ordering::SeqCst);
        let cancelled = scenario == 4;
        let newer_attempt_completed = scenario == 9 && current;
        let mut response = json!({
            "id":RUN_ID,"run_attempt":state.latest_attempt.load(Ordering::SeqCst),
            "event":"push","head_sha":SHA,"status":if cancelled || newer_attempt_completed { "completed" } else { "in_progress" },"conclusion":if cancelled { json!("cancelled") } else if newer_attempt_completed { json!("success") } else { Value::Null },
            "repository":{"id":REPOSITORY_ID,"name":REPOSITORY,"full_name":"example/outrider","owner":{"login":OWNER}},
            "head_repository":{"id":REPOSITORY_ID,"name":REPOSITORY,"full_name":"example/outrider","owner":{"login":OWNER}}
        });
        response["run_attempt"] = json!(attempt);
        match scenario {
            5 => {
                if let Some(fields) = response.as_object_mut() {
                    fields.remove("status");
                }
            }
            6 => {
                response["status"] = json!("unknown");
            }
            7 => {
                response["status"] = json!("completed");
                response["conclusion"] = Value::Null;
            }
            _ => {}
        };
        response
    };
    let job = |job_id, attempt, runner_name| {
        let variant = state.scenario.load(Ordering::SeqCst);
        let (conclusion, runner_name, runner_id, started_at, completed_at) = match variant {
            1 => (
                "success",
                Some("runner-rerun"),
                Some(9001),
                "2020-01-01T00:00:00Z",
                "2020-01-01T00:01:00Z",
            ),
            2 => (
                "failure",
                None,
                None,
                "2020-01-01T00:00:00Z",
                "2020-01-01T00:01:00Z",
            ),
            3 => (
                "failure",
                Some("runner-rerun"),
                Some(9001),
                "2020-01-01T00:00:00Z",
                "2020-01-01T00:01:00Z",
            ),
            _ => (
                "failure",
                Some(runner_name),
                Some(9001),
                "2020-01-01T00:00:00Z",
                "2020-01-01T00:01:00Z",
            ),
        };
        json!({
            "id":job_id,"run_id":RUN_ID,"run_attempt":attempt,"head_sha":SHA,
            "status":"completed","conclusion":conclusion,"runner_name":runner_name,
            "runner_id":runner_id,"started_at":started_at,"completed_at":completed_at
        })
    };
    let (status, body) = if !authorized {
        (401, json!({"message":"fixture token required"}))
    } else {
        match (method.as_str(), path.as_str()) {
            ("GET", "/repos/example/outrider/actions/runs/201") => {
                (200, run(state.latest_attempt.load(Ordering::SeqCst), true))
            }
            ("GET", "/repos/example/outrider/actions/runs/201/attempts/1") => {
                // Keep archived attempt responses separate from the live run.
                (200, run(1, false))
            }
            ("GET", "/repos/example/outrider/actions/runs/201/attempts/2") => {
                // Keep archived attempt responses separate from the live run.
                (200, run(2, false))
            }
            ("GET", path)
                if path
                    == format!(
                        "/repos/example/outrider/actions/runs/{RUN_ID}/attempts/1/jobs?per_page=100&page=1"
                    ) =>
            {
                let (job_id, runner) = if state.scenario.load(Ordering::SeqCst) == 9 {
                    (JOB_ID, "runner-rerun-1".to_owned())
                } else {
                    let job_id = state.job_id.load(Ordering::SeqCst);
                    let runner = rerun_runner_name(job_id);
                    (job_id, runner)
                };
                (200, json!({"jobs":[job(job_id, 1, &runner)]}))
            }
            ("GET", path)
                if path
                    == format!(
                        "/repos/example/outrider/actions/runs/{RUN_ID}/attempts/2/jobs?per_page=100&page=1"
                    ) =>
            {
                (200, json!({"jobs":[job(302, 2, "runner-rerun-2")]}))
            }
            ("GET", path)
                if path
                    == format!(
                        "/repos/example/outrider/actions/jobs/{}",
                        state.job_id.load(Ordering::SeqCst)
                    ) =>
            {
                let job_id = state.job_id.load(Ordering::SeqCst);
                if state.scenario.load(Ordering::SeqCst) == 9 && job_id == 302 {
                    (200, job(302, 2, "runner-rerun-2"))
                } else if state.scenario.load(Ordering::SeqCst) == 9 {
                    (200, job(JOB_ID, 1, "runner-rerun-1"))
                } else {
                    let runner = rerun_runner_name(job_id);
                    (200, job(job_id, 1, &runner))
                }
            }
            ("GET", "/repos/example/outrider/actions/jobs/301") => {
                (200, job(301, 1, "runner-rerun-1"))
            }
            ("GET", "/repos/example/outrider/actions/jobs/302") => {
                (200, job(302, 2, "runner-rerun-2"))
            }
            ("POST", path)
                if path
                    == format!(
                        "/repos/example/outrider/actions/jobs/{}/rerun",
                        state.job_id.load(Ordering::SeqCst)
                    ) =>
            {
                if state.drop_rerun_response.load(Ordering::SeqCst) {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "fixture drops response after recording POST",
                    )
                    .into());
                }

                (
                    state.rerun_status.load(Ordering::SeqCst) as u16,
                    Value::Null,
                )
            }
            _ => (404, json!({"message":"not found"})),
        }
    };
    let body = if method == "POST" && path.ends_with("/rerun") {
        bytes::Bytes::new()
    } else {
        bytes::Bytes::from(body.to_string())
    };
    Ok(Response::builder()
        .status(status)
        .body(http_body_util::Full::new(body))
        .unwrap_or_else(|_| Response::new(http_body_util::Full::new(bytes::Bytes::new()))))
}

pub(crate) fn build_settings(fixture: &RecoveryFixture, origin: &str) -> IngressSettings {
    let github_token = fixture.directory.join("github-token");
    let garm_token = fixture.directory.join("garm-token");
    let webhook_secret = fixture.directory.join("webhook-secret");
    fs::write(&github_token, b"github-fixture-token")
        .unwrap_or_else(|_| panic!("write GitHub token"));
    fs::write(&garm_token, b"garm-fixture-token").unwrap_or_else(|_| panic!("write GARM token"));
    fs::write(&webhook_secret, b"webhook-fixture-secret")
        .unwrap_or_else(|_| panic!("write webhook secret"));
    IngressSettings {
        github_api_origin: origin.to_owned(),
        github_token_file: github_token.display().to_string(),
        garm_api_origin: "http://127.0.0.1:1/".to_owned(),
        garm_token_file: garm_token.display().to_string(),
        controller_id: "00000000-0000-0000-0000-000000000901".to_owned(),
        provider_name: "outrider".to_owned(),
        pool_flavor: "standard".to_owned(),
        pool_image: "ubuntu".to_owned(),
        runner_prefix: "runner".to_owned(),
        max_dynamic_pools: 4,
        max_runners_per_pool: 4,
        allocation_timeout_seconds: 300,
        bootstrap_timeout_minutes: 20,
        timeout_margin_seconds: 30,
        pool_ttl_seconds: 3600,
        webhook_max_bytes: 1024 * 1024,
        repositories: [(
            REPOSITORY_ID.to_string(),
            IngressRepository {
                garm_repository_id: "00000000-0000-0000-0000-000000000101".to_owned(),
                webhook_secret_file: webhook_secret.display().to_string(),
                allow_forks: false,
                template_id: None,
            },
        )]
        .into_iter()
        .collect(),
    }
}

pub(crate) fn seed_job_and_assignment(
    _fixture: &RecoveryFixture,
    journal: &outrider::journal::Journal,
    settings: &IngressSettings,
    interruption_at: Option<&str>,
) -> String {
    seed_job_and_assignment_for_attempt(
        journal,
        settings,
        interruption_at,
        RerunAssignmentFixture {
            run_id: RUN_ID,
            run_attempt: 1,
            job_id: JOB_ID,
            snapshot_job_id: JOB_ID,
            snapshot_id: "snapshot-rerun",
            allocation_request: "allocation-rerun",
            source_id: "recovery-rerun",
            runner_name: "runner-rerun",
        },
    )
}

pub(crate) fn seed_job_and_assignment_for_attempt(
    journal: &outrider::journal::Journal,
    settings: &IngressSettings,
    interruption_at: Option<&str>,
    assignment: RerunAssignmentFixture<'_>,
) -> String {
    let mut central = ConfigPatch::from_toml(include_str!("../../examples/central.toml"))
        .unwrap_or_else(|_| panic!("load valid central snapshot config"));
    central.ingress = Some(settings.clone());
    let mut document = super::allocation_harness::snapshot_document(central, assignment.source_id);
    document.base_repository = RepositoryRef {
        id: REPOSITORY_ID,
        name: REPOSITORY.to_owned(),
        full_name: "example/outrider".to_owned(),
        owner: OwnerRef {
            login: OWNER.to_owned(),
        },
    };
    document.base_repository_id = REPOSITORY_ID;
    document.source_repository_id = REPOSITORY_ID;
    document.run_id = assignment.run_id;
    document.run_attempt = assignment.run_attempt;
    document.job_id = assignment.snapshot_job_id;
    document.commit_sha = SHA.to_owned();
    journal
        .save_pool_snapshot(&PoolSnapshot {
            id: assignment.snapshot_id.to_owned(),
            repository_id: REPOSITORY_ID,
            request_token: assignment.allocation_request.to_owned(),
            config_json: serde_json::to_string(&document)
                .unwrap_or_else(|_| panic!("encode rerun snapshot")),
            state: "enabled".to_owned(),
            garm_pool_id: Some("pool-rerun".to_owned()),
            expires_at: 2_000_000_000,
        })
        .unwrap_or_else(|_| panic!("persist rerun snapshot"));
    let (allocation, _) = journal
        .reserve_allocation(
            assignment.allocation_request,
            "00000000-0000-0000-0000-000000000901",
            assignment.runner_name,
            assignment.snapshot_id,
            "pool-rerun",
        )
        .unwrap_or_else(|_| panic!("reserve rerun allocation"));
    assert!(
        journal
            .claim_allocation_selection(&allocation.allocation_id)
            .unwrap_or_else(|_| panic!("claim allocation"))
    );
    journal
        .set_allocation_selection(&allocation.allocation_id, "{}", "{}", "account", "catalog")
        .unwrap_or_else(|_| panic!("select allocation"));
    journal
        .insert_child_attempt(&ChildAttemptRecord {
            allocation_id: allocation.allocation_id.clone(),
            attempt_number: 1,
            state: "reserved".to_owned(),
            offer_json: "{}".to_owned(),
            binding_json: "{}".to_owned(),
            detail: None,
            created_at: 1_577_836_700,
            dispatched_at: None,
            completed_at: None,
        })
        .unwrap_or_else(|_| panic!("insert child attempt"));
    assert!(
        journal
            .mark_child_dispatched(
                &allocation.allocation_id,
                1,
                &outrider::journal::AllocationAdmission {
                    snapshot_id: assignment.snapshot_id.to_owned(),
                    repository_id: REPOSITORY_ID,
                    run_id: assignment.run_id,
                    run_attempt: assignment.run_attempt,
                    now: 1_577_836_700,
                },
            )
            .unwrap_or_else(|_| panic!("dispatch child"))
    );
    let identity = PlacementIdentity {
        controller_id: "00000000-0000-0000-0000-000000000901".to_owned(),
        cloud: "aws".to_owned(),
        account_id: "account".to_owned(),
        location: "us-east-1".to_owned(),
        resource_id: format!("instance-{}", assignment.runner_name),
        host_id: None,
        dependency_ids: std::collections::BTreeMap::new(),
    };
    let identity_json =
        serde_json::to_string(&identity).unwrap_or_else(|_| panic!("encode identity"));
    assert!(
        journal
            .complete_allocation(&allocation.allocation_id, "{}", &identity_json)
            .unwrap_or_else(|_| panic!("complete runner allocation"))
    );
    if let Some(occurred_at) = interruption_at {
        journal
            .record_interruption(
                &InterruptionRecord {
                    allocation_id: allocation.allocation_id.clone(),
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
                    occurred_at: occurred_at.to_owned(),
                    observed_at: occurred_at.to_owned(),
                    reason_fields_json: "{}".to_owned(),
                },
                &identity,
            )
            .unwrap_or_else(|_| panic!("record runner interruption"));
    }
    journal
        .record_job_observation(&JobObservationRecord {
            repository_id: REPOSITORY_ID,
            repository_owner: OWNER.to_owned(),
            repository_name: REPOSITORY.to_owned(),
            run_id: assignment.run_id,
            run_attempt: assignment.run_attempt,
            job_id: assignment.job_id,
            head_sha: SHA.to_owned(),
            status: "completed".to_owned(),
            conclusion: Some("failure".to_owned()),
            run_status: "in_progress".to_owned(),
            run_conclusion: None,
            runner_name: Some(assignment.runner_name.to_owned()),
            runner_id: Some(9001),
            started_at: Some("2020-01-01T00:00:00Z".to_owned()),
            completed_at: Some("2020-01-01T00:01:00Z".to_owned()),
        })
        .unwrap_or_else(|_| panic!("persist completed job observation"));
    allocation.allocation_id
}

pub(crate) fn build_service(
    settings: IngressSettings,
    journal: Arc<outrider::journal::Journal>,
    enabled: bool,
) -> IngressService {
    IngressService::new(
        ConfigPatch {
            ingress: Some(settings),
            reruns: Some(RerunSettings {
                enabled,
                max_per_run: 2,
                actions_write_permission: enabled,
            }),
            ..ConfigPatch::default()
        },
        journal,
    )
    .unwrap_or_else(|_| panic!("construct rerun ingress"))
}

#[path = "rerun_http/rerun_scenarios.rs"]
mod rerun_scenarios;
pub(crate) use rerun_scenarios::*;
#[path = "rerun_http/retention_iteration.rs"]
mod retention_iteration;
pub(crate) use retention_iteration::*;
