use super::*;

pub(super) type HookMac = Hmac<Sha256>;

pub(super) const WEBHOOK_SECRET: &[u8] = b"system-contract-webhook-secret";
pub(super) const GITHUB_TOKEN: &str = "synthetic-github-token";
pub(super) const GARM_TOKEN: &str = "synthetic-garm-token";
pub(super) const DAEMON_TOKEN: &str = "synthetic-daemon-token";
pub(super) const CONTROLLER_ID: &str = "00000000-0000-0000-0000-000000000901";
pub(super) const REPOSITORY_ID: &str = "00000000-0000-0000-0000-000000000101";
pub(super) const SHA: &str = "1111111111111111111111111111111111111111";
pub(super) const ROUTING_LABEL: &str = "outrider:v1:101:201:1:linux-large";

#[derive(Clone)]
pub(super) struct ApiState {
    pub(super) requests: Arc<Mutex<Vec<ApiRequest>>>,
    pub(super) pool: Arc<Mutex<Option<Value>>>,
    pub(super) pools_by_id: Arc<Mutex<BTreeMap<String, Value>>>,
    pub(super) forwarded: Arc<AtomicBool>,
    pub(super) database: PathBuf,
    pub(super) snapshot_observations: Arc<Mutex<Vec<SnapshotObservation>>>,
    pub(super) job_labels: Arc<Mutex<Vec<String>>>,
    pub(super) current_job_id: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SnapshotCheckpoint {
    PoolEnable,
    Forward,
}

#[derive(Clone, Debug)]
pub(super) struct SnapshotEvidence {
    pub(super) id: String,
    pub(super) repository_id: u64,
    pub(super) request_token: String,
    pub(super) config_json: String,
    pub(super) state: String,
    pub(super) garm_pool_id: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct SnapshotObservation {
    pub(super) checkpoint: SnapshotCheckpoint,
    pub(super) evidence: Option<SnapshotEvidence>,
    pub(super) error: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct ApiRequest {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) body: Vec<u8>,
    pub(super) authorization: Option<String>,
    pub(super) signature: Option<String>,
}

pub(super) struct DependencyFixture {
    pub(super) origin: String,
    pub(super) state: ApiState,
    pub(super) server: JoinHandle<io::Result<()>>,
}

impl DependencyFixture {
    pub(super) async fn start(database: PathBuf) -> io::Result<Self> {
        let listener = TokioTcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}/", listener.local_addr()?);
        let state = ApiState {
            requests: Arc::new(Mutex::new(Vec::new())),
            pool: Arc::new(Mutex::new(None)),
            pools_by_id: Arc::new(Mutex::new(BTreeMap::new())),
            forwarded: Arc::new(AtomicBool::new(false)),
            database,
            snapshot_observations: Arc::new(Mutex::new(Vec::new())),
            job_labels: Arc::new(Mutex::new(vec![ROUTING_LABEL.to_owned()])),
            current_job_id: Arc::new(AtomicU64::new(301)),
        };
        let app = Router::new()
            .fallback(any(dependency_request))
            .with_state(state.clone());
        let server =
            tokio::spawn(async move { axum::serve(listener, app).await.map_err(io::Error::other) });
        Ok(Self {
            origin,
            state,
            server,
        })
    }

    pub(super) fn requests(&self) -> Vec<ApiRequest> {
        self.state
            .requests
            .lock()
            .map(|rows| rows.clone())
            .unwrap_or_default()
    }

    pub(super) fn snapshot_observations(&self) -> Vec<SnapshotObservation> {
        self.state
            .snapshot_observations
            .lock()
            .map(|observations| observations.clone())
            .unwrap_or_default()
    }

    pub(super) fn set_job_label(&self, label: &str) -> io::Result<u64> {
        *self
            .state
            .job_labels
            .lock()
            .map_err(|_| io::Error::other("job labels lock poisoned"))? = vec![label.to_owned()];
        self.state.forwarded.store(false, Ordering::SeqCst);
        let job_id = self.state.current_job_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.state
            .snapshot_observations
            .lock()
            .map_err(|_| io::Error::other("snapshot observations lock poisoned"))?
            .clear();
        Ok(job_id)
    }

    pub(super) async fn wait_forwarded(&self) -> io::Result<()> {
        for _ in 0..200 {
            if self.state.forwarded.load(Ordering::SeqCst) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "signed event was not forwarded",
        ))
    }
}

impl Drop for DependencyFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

pub(super) async fn dependency_request(
    State(state): State<ApiState>,
    request: Request<Body>,
) -> Response<Body> {
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
    let Ok(body) = to_bytes(request.into_body(), 2 * 1024 * 1024).await else {
        return response(StatusCode::BAD_REQUEST, json!({}));
    };
    let body_bytes = body.to_vec();
    if let Ok(mut requests) = state.requests.lock() {
        requests.push(ApiRequest {
            method: method.clone(),
            path: path.clone(),
            body: body_bytes.clone(),
            authorization: authorization.clone(),
            signature,
        });
    }
    let expected_authorization = if path.starts_with("/repos/") {
        Some(format!("Bearer {GITHUB_TOKEN}"))
    } else if path.starts_with("/api/v1/") {
        Some(format!("Bearer {GARM_TOKEN}"))
    } else {
        None
    };
    if expected_authorization
        .as_deref()
        .is_some_and(|expected| authorization.as_deref() != Some(expected))
    {
        return response(StatusCode::UNAUTHORIZED, json!({"error":"unauthorized"}));
    }
    match (method.as_str(), path.as_str()) {
        ("GET", "/repos/base-owner/base-repo/actions/runs/201") => {
            response(StatusCode::OK, github_run())
        }
        ("GET", "/repos/base-owner/base-repo/actions/runs/201/attempts/1") => {
            response(StatusCode::OK, github_run())
        }
        (
            "GET",
            "/repos/base-owner/base-repo/actions/runs/201/attempts/1/jobs?per_page=100&page=1",
        ) => response(StatusCode::OK, json!({"jobs":[github_job(&state)]})),
        ("GET", path) if path.starts_with("/repos/base-owner/base-repo/actions/jobs/") => {
            response(StatusCode::OK, github_job(&state))
        }
        ("GET", "/repos/base-owner/base-repo/commits/1111111111111111111111111111111111111111") => {
            response(StatusCode::OK, json!({"sha":SHA}))
        }
        ("GET", path)
            if path.starts_with("/repos/base-owner/base-repo/contents/.outrider.yml?") =>
        {
            response(StatusCode::NOT_FOUND, json!({"message":"not found"}))
        }
        ("GET", path) if path.contains("/check-runs?") => {
            response(StatusCode::OK, json!({"check_runs":[]}))
        }
        ("POST", "/repos/base-owner/base-repo/check-runs") => {
            response(StatusCode::CREATED, json!({"id":9001}))
        }
        ("GET", "/api/v1/repositories/00000000-0000-0000-0000-000000000101") => response(
            StatusCode::OK,
            json!({"id":REPOSITORY_ID,"owner":"base-owner","name":"base-repo"}),
        ),
        ("GET", "/api/v1/repositories/00000000-0000-0000-0000-000000000101/pools") => {
            let pools = state
                .pools_by_id
                .lock()
                .ok()
                .map(|pools| pools.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            response(StatusCode::OK, json!(pools))
        }
        ("POST", "/api/v1/repositories/00000000-0000-0000-0000-000000000101/pools") => {
            let create: Value = serde_json::from_slice(&body_bytes).unwrap_or_default();
            let tags = create["tags"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|tag| json!({"name":tag}))
                .collect::<Vec<_>>();
            let request_token = create["tags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .find(|tag| tag.starts_with("outrider:v1:"))
                .unwrap_or(ROUTING_LABEL);
            let pool_id = if request_token.ends_with(":linux-gcp") {
                "pool-system-202"
            } else if request_token.ends_with(":linux-azure") {
                "pool-system-203"
            } else {
                "pool-system-201"
            };
            let pool = json!({"id":pool_id,"repo_id":REPOSITORY_ID,"provider_name":create["provider_name"],"enabled":false,"max_runners":create["max_runners"],"min_idle_runners":create["min_idle_runners"],"tags":tags,"image":create["image"],"flavor":create["flavor"],"runner_prefix":create["runner_prefix"],"os_type":create["os_type"],"os_arch":create["os_arch"],"runner_bootstrap_timeout":create["runner_bootstrap_timeout"],"extra_specs":create["extra_specs"],"instances":[]});
            if let Ok(mut current) = state.pool.lock() {
                *current = Some(pool.clone());
            }
            if let Ok(mut pools) = state.pools_by_id.lock() {
                pools.insert(pool_id.to_owned(), pool.clone());
            }
            response(StatusCode::OK, pool)
        }
        ("PUT", path)
            if path.starts_with(
                "/api/v1/repositories/00000000-0000-0000-0000-000000000101/pools/",
            ) =>
        {
            let pool_id = path.rsplit('/').next().unwrap_or_default();
            let mut pool = state
                .pools_by_id
                .lock()
                .ok()
                .and_then(|pools| pools.get(pool_id).cloned())
                .unwrap_or_default();
            let request_token = pool["tags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|tag| tag["name"].as_str())
                .find(|tag| tag.starts_with("outrider:v1:"))
                .unwrap_or(ROUTING_LABEL);
            capture_snapshot(&state, SnapshotCheckpoint::PoolEnable, request_token);
            pool["enabled"] = json!(true);
            if let Ok(mut pools) = state.pools_by_id.lock() {
                pools.insert(pool_id.to_owned(), pool.clone());
            }
            if let Ok(mut current) = state.pool.lock() {
                *current = Some(pool.clone());
            }
            response(StatusCode::OK, pool)
        }
        ("POST", "/webhooks/00000000-0000-0000-0000-000000000901") => {
            let pool = state
                .pool
                .lock()
                .ok()
                .and_then(|pool| pool.clone())
                .unwrap_or_default();
            let request_token = pool["tags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|tag| tag["name"].as_str())
                .find(|tag| tag.starts_with("outrider:v1:"))
                .unwrap_or(ROUTING_LABEL);
            capture_snapshot(&state, SnapshotCheckpoint::Forward, request_token);
            state.forwarded.store(true, Ordering::SeqCst);
            Response::builder()
                .status(StatusCode::OK)
                .body(Body::empty())
                .unwrap_or_else(|_| Response::new(Body::empty()))
        }
        ("GET", path) if path.starts_with("/api/v1/pools/") && path.ends_with("/instances") => {
            response(StatusCode::OK, json!([]))
        }
        ("DELETE", path)
            if path.starts_with(
                "/api/v1/repositories/00000000-0000-0000-0000-000000000101/pools/",
            ) =>
        {
            let pool_id = path.rsplit('/').next().unwrap_or_default();
            if let Ok(mut pools) = state.pools_by_id.lock() {
                pools.remove(pool_id);
            }
            response(StatusCode::OK, json!({}))
        }
        _ => response(StatusCode::NOT_FOUND, json!({"error":"not found"})),
    }
}

pub(super) fn capture_snapshot(
    state: &ApiState,
    checkpoint: SnapshotCheckpoint,
    request_token: &str,
) {
    let (evidence, error) = match read_snapshot(&state.database, request_token) {
        Ok(evidence) => (evidence, None),
        Err(error) => (None, Some(error.to_string())),
    };
    if let Ok(mut observations) = state.snapshot_observations.lock() {
        observations.push(SnapshotObservation {
            checkpoint,
            evidence,
            error,
        });
    }
}

pub(super) fn read_snapshot(
    database: &std::path::Path,
    request_token: &str,
) -> rusqlite::Result<Option<SnapshotEvidence>> {
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection
        .query_row(
            "SELECT id, repository_id, request_token, config_json, state, garm_pool_id FROM pool_snapshots WHERE repository_id = 101 AND request_token = ?1",
            [request_token],
            |row| {
                let repository_id = u64::try_from(row.get::<_, i64>(1)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?;
                Ok(SnapshotEvidence {
                    id: row.get(0)?,
                    repository_id,
                    request_token: row.get(2)?,
                    config_json: row.get(3)?,
                    state: row.get(4)?,
                    garm_pool_id: row.get(5)?,
                })
            },
        )
        .optional()
}

pub(super) fn verify_snapshot(
    snapshot: &SnapshotEvidence,
    expected_os: &str,
    expected_architecture: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let document: outrider::github::SnapshotDocument = serde_json::from_str(&snapshot.config_json)?;
    if snapshot.id.is_empty()
        || snapshot.repository_id != 101
        || snapshot.request_token != ROUTING_LABEL
        || document.base_repository_id != 101
        || document.commit_sha != SHA
        || document.resolved_profile.os != expected_os
        || document.resolved_profile.architecture != expected_architecture
    {
        return Err(format!(
            "signed snapshot differs from expected repository, SHA, or profile: {snapshot:?}"
        )
        .into());
    }
    if expected_os == "macos"
        && !document.central.limits.as_ref().is_some_and(|limits| {
            limits.allowed_os.contains("macos") && limits.allowed_architectures.contains("arm64")
        })
    {
        return Err("signed MacARM64 snapshot omitted explicit administrator limits".into());
    }
    Ok(())
}

pub(super) fn github_run() -> Value {
    let repository = json!({"id":101,"name":"base-repo","full_name":"base-owner/base-repo","owner":{"login":"base-owner"}});
    json!({"id":201,"run_attempt":1,"event":"push","head_sha":SHA,"status":"queued","conclusion":null,"repository":repository,"head_repository":repository})
}

pub(super) fn github_job(state: &ApiState) -> Value {
    let labels = state
        .job_labels
        .lock()
        .map(|labels| labels.clone())
        .unwrap_or_default();
    json!({"id":state.current_job_id.load(Ordering::SeqCst),"run_id":201,"run_attempt":1,"head_sha":SHA,"status":"queued","conclusion":null,"labels":labels})
}

pub(super) fn response(status: StatusCode, value: Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}
