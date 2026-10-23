use super::*;

pub(super) type TestMac = Hmac<Sha256>;

pub(super) const REPOSITORY_ID: u64 = 101;
pub(super) const GARM_REPOSITORY_ID: &str = "00000000-0000-0000-0000-000000000101";
pub(super) const CONTROLLER_ID: &str = "00000000-0000-0000-0000-000000000901";
pub(super) const TOKEN: &str = "outrider:v1:101:201:1:build";
pub(super) const SHA: &str = "1111111111111111111111111111111111111111";
pub(super) const SECRET: &[u8] = b"synthetic-webhook-secret";
pub(super) static DIRECTORY_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub(super) struct TestDirectory {
    pub(super) path: PathBuf,
}

impl TestDirectory {
    pub(super) fn new() -> io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let sequence = DIRECTORY_SEQUENCE.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "outrider-ingress-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub(super) fn write(&self, name: &str, contents: &[u8]) -> io::Result<PathBuf> {
        let path = self.path.join(name);
        fs::write(&path, contents)?;
        Ok(path)
    }

    pub(super) fn journal(&self) -> Result<Arc<Journal>, outrider::journal::JournalError> {
        Journal::open(&self.path.join("journal.sqlite3")).map(Arc::new)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _result = fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, Debug)]
pub(super) struct RecordedRequest {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) body: Vec<u8>,
    pub(super) authorization: Option<String>,
    pub(super) signature: Option<String>,
}

#[derive(Clone, Copy)]
pub(super) enum FixtureMode {
    GarmPool,
    GitHubFork,
    GitHubMatrix,
}

pub(super) struct FixtureState {
    pub(super) mode: FixtureMode,
    pub(super) requests: Mutex<Vec<RecordedRequest>>,
    pub(super) pool_lists: AtomicUsize,
    pub(super) github_run: Option<Value>,
    pub(super) current_run_override: Mutex<Option<Value>>,
    pub(super) github_job: Option<Value>,
    pub(super) github_job_list_override: Mutex<Option<Value>>,
    pub(super) github_job_override: Mutex<Option<Value>>,
    pub(super) latest_run_attempt: AtomicUsize,
    pub(super) rerun_ack_status: AtomicUsize,
    pub(super) commit_visible: bool,
    pub(super) created_pool: Mutex<Option<Value>>,
    pub(super) journal: Option<Arc<Journal>>,
    pub(super) snapshot_visible_at_create: AtomicBool,
    pub(super) checks: Mutex<Vec<Value>>,
    pub(super) fail_check_after_store: AtomicBool,
    pub(super) fail_pool_list_after_create: AtomicBool,
    pub(super) terminal_during_pool_create: AtomicBool,
    pub(super) fail_enable_after_store: AtomicBool,
    pub(super) force_empty_pool_inventory: AtomicBool,
    pub(super) pool_detail_status: AtomicUsize,
    pub(super) delete_pool_removes_remote: AtomicBool,
}

pub(super) struct HttpFixture {
    pub(super) origin: String,
    pub(super) state: Arc<FixtureState>,
    pub(super) task: JoinHandle<io::Result<()>>,
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl HttpFixture {
    pub(super) async fn start(mode: FixtureMode) -> io::Result<Self> {
        Self::start_with_data(mode, None, None, true, None).await
    }

    pub(super) async fn start_garm(journal: Arc<Journal>) -> io::Result<Self> {
        Self::start_with_data(FixtureMode::GarmPool, None, None, true, Some(journal)).await
    }

    pub(super) async fn start_github(
        run: Value,
        job: Value,
        commit_visible: bool,
    ) -> io::Result<Self> {
        Self::start_with_data(
            FixtureMode::GitHubMatrix,
            Some(run),
            Some(job),
            commit_visible,
            None,
        )
        .await
    }

    pub(super) async fn start_with_data(
        mode: FixtureMode,
        github_run: Option<Value>,
        github_job: Option<Value>,
        commit_visible: bool,
        journal: Option<Arc<Journal>>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}/", listener.local_addr()?);
        let state = Arc::new(FixtureState {
            mode,
            requests: Mutex::new(Vec::new()),
            pool_lists: AtomicUsize::new(0),
            github_run,
            current_run_override: Mutex::new(None),
            github_job_list_override: Mutex::new(None),
            latest_run_attempt: AtomicUsize::new(1),
            rerun_ack_status: AtomicUsize::new(201),
            github_job,
            github_job_override: Mutex::new(None),
            commit_visible,
            created_pool: Mutex::new(None),
            journal,
            snapshot_visible_at_create: AtomicBool::new(false),
            checks: Mutex::new(Vec::new()),
            fail_check_after_store: AtomicBool::new(false),
            fail_pool_list_after_create: AtomicBool::new(false),
            terminal_during_pool_create: AtomicBool::new(false),
            fail_enable_after_store: AtomicBool::new(false),
            force_empty_pool_inventory: AtomicBool::new(false),
            pool_detail_status: AtomicUsize::new(0),
            delete_pool_removes_remote: AtomicBool::new(false),
        });
        let shared = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await?;
                let state = shared.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| handle_request(request, state.clone()));
                    let _result = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Ok(Self {
            origin,
            state,
            task,
        })
    }

    pub(super) fn requests(&self) -> Vec<RecordedRequest> {
        self.state
            .requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }

    pub(super) fn set_github_job(&self, job: Value) {
        if let Ok(mut override_job) = self.state.github_job_override.lock() {
            *override_job = Some(job);
        }
    }

    pub(super) fn set_github_job_list(&self, job: Value) {
        if let Ok(mut override_job) = self.state.github_job_list_override.lock() {
            *override_job = Some(job);
        }
    }

    pub(super) fn set_current_run(&self, run: Value) {
        if let Ok(mut override_run) = self.state.current_run_override.lock() {
            *override_run = Some(run);
        }
    }
}

pub(super) async fn handle_request(
    request: Request<Incoming>,
    state: Arc<FixtureState>,
) -> Result<Response<Full<bytes::Bytes>>, hyper::Error> {
    // GitHub rejects requests without a valid User-Agent, even with valid auth.
    if !matches!(state.mode, FixtureMode::GarmPool)
        && request
            .headers()
            .get("user-agent")
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| value.trim().is_empty())
    {
        let mut response = Response::new(Full::new(bytes::Bytes::from_static(
            b"User-Agent header is required",
        )));
        *response.status_mut() = StatusCode::FORBIDDEN;
        return Ok(response);
    }

    let method = request.method().to_string();
    let path = request.uri().path_and_query().map_or_else(
        || request.uri().path().to_owned(),
        |value| value.as_str().to_owned(),
    );
    let authorization = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let signature = request
        .headers()
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = request.into_body().collect().await?.to_bytes().to_vec();
    if let Ok(mut requests) = state.requests.lock() {
        requests.push(RecordedRequest {
            method: method.clone(),
            path: path.clone(),
            body: body.clone(),
            authorization,
            signature,
        });
    }
    let (status, response_body) = match state.mode {
        FixtureMode::GarmPool => handle_garm_request(&method, &path, &body, &state),
        FixtureMode::GitHubFork => match path.as_str() {
            "/repos/base-owner/base-repo/actions/runs/201/attempts/1" => (200, fork_run()),
            "/repos/base-owner/base-repo/actions/runs/201/attempts/1/jobs?per_page=100&page=1" => {
                (200, json!({"jobs":[job_response()]}))
            }
            "/repos/base-owner/base-repo/actions/jobs/301" => (200, job_response()),
            _ => (404, json!({"error":"not found"})),
        },
        FixtureMode::GitHubMatrix => {
            if method == "POST" && path == "/repos/base-owner/base-repo/actions/jobs/301/rerun" {
                (
                    state.rerun_ack_status.load(Ordering::SeqCst) as u16,
                    json!({}),
                )
            } else if method == "POST" && path == "/repos/base-owner/base-repo/check-runs" {
                let check: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
                if let Ok(mut checks) = state.checks.lock() {
                    checks.push(json!({"external_id":check["external_id"]}));
                }
                let status = if state.fail_check_after_store.swap(false, Ordering::SeqCst) {
                    500
                } else {
                    201
                };
                (status, json!({"external_id":check["external_id"]}))
            } else if method == "GET" && path.contains("/check-runs?") {
                let checks = state
                    .checks
                    .lock()
                    .map(|checks| checks.clone())
                    .unwrap_or_default();
                (200, json!({"check_runs":checks}))
            } else {
                let run = state.github_run.clone().unwrap_or_default();
                let job = state
                    .github_job_override
                    .lock()
                    .ok()
                    .and_then(|override_job| override_job.clone())
                    .or_else(|| state.github_job.clone())
                    .unwrap_or_default();
                let listed_job = state
                    .github_job_list_override
                    .lock()
                    .ok()
                    .and_then(|override_job| override_job.clone())
                    .unwrap_or_else(|| job.clone());
                let run_id = run["id"].as_u64().unwrap_or_default();
                let attempt = run["run_attempt"].as_u64().unwrap_or_default();
                let job_id = job["id"].as_u64().unwrap_or_default();
                let sha = run["head_sha"].as_str().unwrap_or_default();
                match path.as_str() {
                    value
                        if method == "GET"
                            && value
                                == format!("/repos/base-owner/base-repo/actions/runs/{run_id}") =>
                    {
                        let mut current = state
                            .current_run_override
                            .lock()
                            .ok()
                            .and_then(|override_run| override_run.clone())
                            .unwrap_or_else(|| run.clone());
                        current["run_attempt"] =
                            json!(state.latest_run_attempt.load(Ordering::SeqCst));
                        (200, current)
                    }
                    value
                        if value
                            == format!(
                                "/repos/base-owner/base-repo/actions/runs/{run_id}/attempts/{attempt}"
                            ) =>
                    {
                        (200, run)
                    }
                    value
                        if value
                            == format!(
                                "/repos/base-owner/base-repo/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100&page=1"
                            ) =>
                    {
                        (200, json!({"jobs":[listed_job]}))
                    }
                    value
                        if value
                            == format!("/repos/base-owner/base-repo/actions/jobs/{job_id}") =>
                    {
                        (200, job)
                    }
                    value if value == format!("/repos/base-owner/base-repo/commits/{sha}") => {
                        if state.commit_visible {
                            (200, json!({"sha":sha}))
                        } else {
                            (404, json!({"message":"not found"}))
                        }
                    }
                    value
                        if value
                            == format!(
                                "/repos/base-owner/base-repo/contents/.outrider.yml?ref={sha}"
                            ) =>
                    {
                        (404, json!({"message":"not found"}))
                    }
                    _ => (404, json!({"error":"not found"})),
                }
            }
        }
    };
    let response_body = if path.ends_with("/rerun") {
        bytes::Bytes::new()
    } else {
        bytes::Bytes::from(response_body.to_string())
    };
    Ok(Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("content-type", "application/json")
        .body(Full::new(response_body))
        .unwrap_or_else(|_| Response::new(Full::new(bytes::Bytes::new()))))
}

pub(super) fn handle_garm_request(
    method: &str,
    path: &str,
    body: &[u8],
    state: &FixtureState,
) -> (u16, Value) {
    match (method, path) {
        ("GET", "/api/v1/repositories/00000000-0000-0000-0000-000000000101") => (
            200,
            json!({"id":GARM_REPOSITORY_ID,"owner":"base-owner","name":"base-repo"}),
        ),
        ("GET", "/api/v1/repositories/00000000-0000-0000-0000-000000000101/pools") => {
            let call = state.pool_lists.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                (200, json!([]))
            } else if state
                .fail_pool_list_after_create
                .swap(false, Ordering::SeqCst)
            {
                (503, json!({"error":"reconcile temporarily unavailable"}))
            } else if state.force_empty_pool_inventory.load(Ordering::SeqCst) {
                (200, json!([]))
            } else {
                let pool = state
                    .created_pool
                    .lock()
                    .ok()
                    .and_then(|pool| pool.clone())
                    .unwrap_or_else(pool_response);
                (200, json!([pool]))
            }
        }
        ("POST", "/api/v1/repositories/00000000-0000-0000-0000-000000000101/pools") => {
            let create: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
            let token = create["tags"][0].as_str().unwrap_or_default();
            let visible = state.journal.as_ref().is_some_and(|journal| {
                journal
                    .pool_snapshot(REPOSITORY_ID, token)
                    .is_ok_and(|snapshot| snapshot.is_some())
            });
            state
                .snapshot_visible_at_create
                .store(visible, Ordering::SeqCst);
            let pool = pool_from_create(&create);
            if let Ok(mut created_pool) = state.created_pool.lock() {
                *created_pool = Some(pool.clone());
            }
            if state
                .terminal_during_pool_create
                .swap(false, Ordering::SeqCst)
                && let Some(journal) = state.journal.as_ref()
            {
                let _result = journal.mark_run_terminal(REPOSITORY_ID, 201, 1);
            }
            (200, pool)
        }
        ("GET", "/api/v1/repositories/00000000-0000-0000-0000-000000000101/pools/pool-201") => {
            let status = state.pool_detail_status.load(Ordering::SeqCst);
            if status != 0 {
                (
                    u16::try_from(status).unwrap_or(500),
                    json!({"error":"pool unavailable"}),
                )
            } else {
                let pool = state
                    .created_pool
                    .lock()
                    .ok()
                    .and_then(|pool| pool.clone())
                    .unwrap_or_else(pool_response);
                (200, pool)
            }
        }
        ("PUT", "/api/v1/repositories/00000000-0000-0000-0000-000000000101/pools/pool-201") => {
            let update: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
            let mut pool = state
                .created_pool
                .lock()
                .ok()
                .and_then(|pool| pool.clone())
                .unwrap_or_else(pool_response);
            if let Some(enabled) = update.get("enabled").and_then(Value::as_bool) {
                pool["enabled"] = json!(enabled);
            }
            if let Ok(mut created_pool) = state.created_pool.lock() {
                *created_pool = Some(pool.clone());
            }
            let status = if update["enabled"] == true
                && state.fail_enable_after_store.swap(false, Ordering::SeqCst)
            {
                500
            } else {
                200
            };
            (status, pool)
        }
        ("GET", "/api/v1/pools/pool-201/instances") => (200, json!([])),
        ("DELETE", "/api/v1/repositories/00000000-0000-0000-0000-000000000101/pools/pool-201") => {
            if state.delete_pool_removes_remote.load(Ordering::SeqCst)
                && let Ok(mut pool) = state.created_pool.lock()
            {
                *pool = None;
            }
            (200, json!({}))
        }
        ("POST", "/webhooks/00000000-0000-0000-0000-000000000901") => (200, json!({})),
        _ => (404, json!({"error":"not found"})),
    }
}

pub(super) fn pool_response() -> Value {
    json!({
        "id":"pool-201", "repo_id":GARM_REPOSITORY_ID, "provider_name":"outrider", "enabled":false,
        "max_runners":4, "min_idle_runners":0, "tags":[{"name":TOKEN}], "image":"garm-image", "flavor":"garm-flavor",
        "runner_prefix":"outrider", "os_type":"linux", "os_arch":"amd64", "runner_bootstrap_timeout":26,
        "extra_specs":{"snapshot_id":snapshot_id()}, "template_id":9, "instances":[]
    })
}

pub(super) fn pool_from_create(create: &Value) -> Value {
    let mut pool = json!({
        "id":"pool-201", "repo_id":GARM_REPOSITORY_ID,
        "provider_name":create["provider_name"], "enabled":false,
        "max_runners":create["max_runners"], "min_idle_runners":create["min_idle_runners"],
        "tags":create["tags"].as_array().into_iter().flatten().map(|tag| json!({"name":tag})).collect::<Vec<_>>(),
        "image":create["image"], "flavor":create["flavor"], "runner_prefix":create["runner_prefix"],
        "os_type":create["os_type"], "os_arch":create["os_arch"],
        "runner_bootstrap_timeout":create["runner_bootstrap_timeout"], "extra_specs":create["extra_specs"],
        "instances":[]
    });
    if let Some(template_id) = create.get("template_id") {
        pool["template_id"] = template_id.clone();
    }
    pool
}

pub(super) fn snapshot_id() -> String {
    let mut digest = sha2::Sha256::new();
    use sha2::Digest;
    digest.update(REPOSITORY_ID.to_be_bytes());
    digest.update(TOKEN.as_bytes());
    hex::encode(digest.finalize())
}

pub(super) fn draining_pool_snapshot(settings: &IngressSettings) -> PoolSnapshot {
    let document = outrider::github::SnapshotDocument {
        base_repository: RepositoryRef {
            id: REPOSITORY_ID,
            name: "base-repo".into(),
            full_name: "base-owner/base-repo".into(),
            owner: OwnerRef {
                login: "base-owner".into(),
            },
        },
        base_repository_id: REPOSITORY_ID,
        source_repository_id: REPOSITORY_ID,
        run_id: 201,
        run_attempt: 1,
        job_id: 301,
        commit_sha: SHA.into(),
        central: config_with_ingress(settings.clone()),
        repository: ConfigPatch::default(),
        resolved_profile: profile(),
        image_catalogs: vec![],
    };
    PoolSnapshot {
        id: snapshot_id(),
        repository_id: REPOSITORY_ID,
        request_token: TOKEN.into(),
        config_json: serde_json::to_string(&document)
            .unwrap_or_else(|_| panic!("snapshot encoding")),
        state: "draining".into(),
        garm_pool_id: Some("pool-201".into()),
        expires_at: 2_000_000_000,
    }
}

pub(super) fn failed_run() -> Value {
    let mut run = push_run();
    run["status"] = json!("in_progress");
    run["conclusion"] = Value::Null;
    run
}

pub(super) fn failed_assigned_job() -> Value {
    json!({
        "id":301,"run_id":201,"run_attempt":1,"head_sha":SHA,
        "status":"completed","conclusion":"failure","runner_name":"runner-301","runner_id":9001,
        "started_at":"2026-10-21T00:00:00Z","completed_at":"2026-10-21T00:01:00Z"
    })
}
