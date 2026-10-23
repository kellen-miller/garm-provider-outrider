use super::*;

pub(super) fn available_address() -> io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.to_string())
}

pub(super) fn strip_null_configuration_fields(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|_, value| !value.is_null());
            for value in fields.values_mut() {
                strip_null_configuration_fields(value);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_null_configuration_fields(item);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

pub(super) fn serialize_reload_and_validate_central_config(
    config: &ConfigPatch,
) -> Result<ConfigPatch, Box<dyn std::error::Error>> {
    let mut config_json = serde_json::to_value(config)?;
    strip_null_configuration_fields(&mut config_json);
    let config_yaml = serde_yaml_ng::to_string(&config_json)?;
    let reloaded = ConfigPatch::from_yaml(&config_yaml)?;
    let yaml_value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&config_yaml)?;
    let serialized_config_json = serde_json::to_value(yaml_value)?;
    let validator = jsonschema::validator_for(&outrider::config::json_schema())?;
    if !validator.is_valid(&serialized_config_json) {
        return Err("Explain-derived central config failed exported JSON schema".into());
    }
    Ok(reloaded)
}

pub(super) async fn start_daemon(
    config: &std::path::Path,
    address: &str,
    aws_endpoint: &str,
    lifecycle_log: &std::path::Path,
) -> io::Result<Child> {
    let stderr_path = lifecycle_log.with_extension("daemon.stderr");
    let stderr = fs::File::create(stderr_path)?;
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(["serve", "--config"])
        .arg(config)
        .env("AWS_ENDPOINT_URL", aws_endpoint)
        .env("AWS_REGION", "us-east-1")
        .env("AWS_ACCESS_KEY_ID", "AKIDTESTAWSCONTRACT")
        .env("AWS_SECRET_ACCESS_KEY", "secret-test-only-not-a-real-key")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("OUTRIDER_FIXTURE_MODE", "record")
        .env("OUTRIDER_NATIVE_LOCATOR", "i-synthetic-001")
        .env(
            "OUTRIDER_CREATE_BARRIER",
            lifecycle_log.with_extension("create-barrier"),
        )
        .env("OUTRIDER_LIFECYCLE_LOG", lifecycle_log)
        .env(
            "OUTRIDER_RECORD",
            lifecycle_log.with_extension("record.json"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()?;
    for _ in 0..100 {
        if daemon_ready(address).await {
            return Ok(daemon);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        if daemon.try_wait()?.is_some() {
            let stderr = fs::read_to_string(lifecycle_log.with_extension("daemon.stderr"))?;
            return Err(io::Error::other(format!(
                "daemon exited before becoming ready: {stderr}"
            )));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "daemon health endpoint did not become ready",
    ))
}

pub(super) async fn daemon_ready(address: &str) -> bool {
    reqwest::get(format!("http://{address}/health"))
        .await
        .is_ok_and(|response| response.status().is_success())
}

pub(super) async fn provider_process(
    config: &std::path::Path,
    command: &str,
    pool: Option<&str>,
    body: Option<&[u8]>,
) -> io::Result<Output> {
    let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_outrider"));
    process
        .env("GARM_COMMAND", command)
        .env("GARM_CONTROLLER_ID", CONTROLLER_ID)
        .env("GARM_INTERFACE_VERSION", "v0.1.0")
        .env("GARM_PROVIDER_CONFIG_FILE", config)
        .env_remove("GARM_POOL_EXTRASPECS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(pool) = pool {
        process.env("GARM_POOL_ID", pool);
    }
    if body.is_some() {
        process.stdin(Stdio::piped());
    }
    let mut child = process.spawn()?;
    if let Some(body) = body {
        child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("provider stdin was not piped"))?
            .write_all(body)
            .await?;
    }
    child.wait_with_output().await
}

pub(super) fn replace_current_provider_files(
    config_path: &std::path::Path,
    directory: &std::path::Path,
) -> Result<BTreeSet<PathBuf>, Box<dyn std::error::Error>> {
    let mut config = ConfigPatch::from_yaml(&fs::read_to_string(config_path)?)?;
    let bindings = config
        .provider_bindings
        .as_mut()
        .ok_or("current provider bindings missing")?;
    let executable_source = outrider_xtask::provider_fixture::build_executable();
    let mut old_files = BTreeSet::new();
    for binding in bindings {
        old_files.insert(binding.executable_path.clone());
        old_files.insert(binding.configuration_path.clone());
        let executable = directory.join(format!("current-{}-provider-process", binding.id));
        fs::copy(&executable_source, &executable)?;
        binding.executable_path = executable.clone();
        binding.executable_sha256 = hex::encode(Sha256::digest(fs::read(&executable)?));
        let configuration = directory.join(format!("current-{}-aws-provider.toml", binding.id));
        fs::copy(&binding.configuration_path, &configuration)?;
        binding.configuration_path = configuration;
        binding.source_revision = "1123456789abcdef0123456789abcdef01234567".to_owned();
    }
    let mut config_value = serde_json::to_value(config)?;
    strip_null_configuration_fields(&mut config_value);
    fs::write(config_path, serde_yaml_ng::to_string(&config_value)?)?;
    Ok(old_files)
}

pub(super) async fn provider_lifecycle_process(
    config: &std::path::Path,
    command: &str,
    pool: Option<&str>,
    instance: Option<&str>,
) -> io::Result<Output> {
    let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_outrider"));
    process
        .env("GARM_COMMAND", command)
        .env("GARM_CONTROLLER_ID", CONTROLLER_ID)
        .env("GARM_INTERFACE_VERSION", "v0.1.0")
        .env("GARM_PROVIDER_CONFIG_FILE", config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(pool) = pool {
        process.env("GARM_POOL_ID", pool);
    }
    if let Some(instance) = instance {
        process.env("GARM_INSTANCE_ID", instance);
    }
    process.output().await
}

pub(super) fn lifecycle_commands(path: &std::path::Path) -> io::Result<Vec<String>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    text.lines()
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .map(|record| record["command"].as_str().unwrap_or_default().to_owned())
                .map_err(io::Error::other)
        })
        .collect()
}

pub(super) async fn assert_explanation_retention_policy_and_limit_diagnostics(
    scenario: &mut SystemScenario,
    snapshot_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut retention_config = ConfigPatch::from_yaml(&fs::read_to_string(&scenario.config_path)?)?;
    let retention_policy = retention_config
        .policies
        .as_mut()
        .and_then(|policies| policies.get_mut("explain-weighted"))
        .ok_or("weighted explanation policy missing")?;
    let outrider::policy::Policy::Weighted(weighted) = &mut retention_policy.strategy else {
        return Err("weighted explanation policy changed strategy".into());
    };
    weighted.max_observation_age_seconds = 120 * 24 * 60 * 60;
    retention_config
        .history
        .as_mut()
        .ok_or("history settings missing")?
        .window_seconds = 120 * 24 * 60 * 60;
    let mut retention_config_value = serde_json::to_value(retention_config)?;
    strip_null_configuration_fields(&mut retention_config_value);
    fs::write(
        &scenario.config_path,
        serde_yaml_ng::to_string(&retention_config_value)?,
    )?;
    let retention_now_millis =
        i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
    let retention_floor_millis = retention_now_millis - 30 * 24 * 60 * 60 * 1000;
    let retention_connection =
        Connection::open(scenario.fixture.directory.join("journal.sqlite3"))?;
    let create_scope: (String, String, String) = retention_connection.query_row(
        "SELECT candidate_key, catalog_fingerprint, provider_pin_json FROM create_history ORDER BY observed_at_millis DESC LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    for dimension in ["latency", "running"] {
        retention_connection.execute(
            "INSERT INTO history_retention_floors(candidate_key, catalog_fingerprint, provider_pin_json, dimension, floor_millis) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(candidate_key, catalog_fingerprint, provider_pin_json, dimension) DO UPDATE SET floor_millis=MAX(floor_millis, excluded.floor_millis)",
            rusqlite::params![create_scope.0, create_scope.1, create_scope.2, dimension, retention_floor_millis],
        )?;
    }
    drop(retention_connection);
    let endpoint = scenario.aws.endpoint.clone();
    scenario.restart(&endpoint).await?;
    let retention_report = tokio::process::Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(["explain", "--config"])
        .arg(&scenario.config_path)
        .args(["--profile", "linux-large"])
        .output()
        .await?;
    if !retention_report.status.success() {
        return Err(format!(
            "expanded-retention explanation failed: {}",
            String::from_utf8_lossy(&retention_report.stderr)
        )
        .into());
    }
    let retention_report: Value = serde_json::from_slice(&retention_report.stdout)?;
    let retained_candidates = retention_report["candidates"]
        .as_array()
        .ok_or("expanded-retention candidates missing")?;
    let has_retention_reason = |needle: &str| {
        retained_candidates.iter().any(|candidate| {
            candidate["history"]["unknown_reasons"]
                .as_array()
                .is_some_and(|reasons| {
                    reasons
                        .iter()
                        .any(|reason| reason.as_str().is_some_and(|value| value.contains(needle)))
                })
        })
    };
    if !has_retention_reason("latency history is incomplete")
        || !has_retention_reason("running exposure history is incomplete")
    {
        return Err(format!(
            "Explain lost expanded-retention coverage diagnostics: {retention_report}"
        )
        .into());
    }
    if !retained_candidates.iter().any(|candidate| {
        candidate["history"]["observation_window"].is_null()
            && candidate["history"]["queried_window"]["start_unix_seconds"]
                .as_u64()
                .is_some_and(|start| start < retention_now_millis as u64 / 1000 - 60 * 24 * 60 * 60)
    }) {
        return Err("empty history omitted its actual query window".into());
    }

    let mut current_config = ConfigPatch::from_yaml(&fs::read_to_string(&scenario.config_path)?)?;
    current_config
        .policies
        .as_mut()
        .and_then(|policies| policies.get_mut("explain-weighted"))
        .ok_or("weighted explanation policy missing")?
        .missing_data = outrider::policy::MissingDataBehavior::Reject;
    let mut current_config_value = serde_json::to_value(&current_config)?;
    strip_null_configuration_fields(&mut current_config_value);
    fs::write(
        &scenario.config_path,
        serde_yaml_ng::to_string(&current_config_value)?,
    )?;
    let endpoint = scenario.aws.endpoint.clone();
    scenario.restart(&endpoint).await?;
    let rejected_report = tokio::process::Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(["explain", "--config"])
        .arg(&scenario.config_path)
        .args(["--profile", "linux-large"])
        .output()
        .await?;
    if !rejected_report.status.success() {
        return Err(format!(
            "structured policy rejection explanation failed: {}",
            String::from_utf8_lossy(&rejected_report.stderr)
        )
        .into());
    }
    let rejected_report: Value = serde_json::from_slice(&rejected_report.stdout)?;
    if rejected_report["policy_error"].as_str().is_none()
        || rejected_report["policy_attempts"][0]["outcome"] != "rejected"
        || rejected_report["candidates"]
            .as_array()
            .is_none_or(Vec::is_empty)
    {
        return Err(format!(
            "Explain lost the actual rejected policy diagnostics: {rejected_report}"
        )
        .into());
    }

    let mut current_config = ConfigPatch::from_yaml(&fs::read_to_string(&scenario.config_path)?)?;
    current_config
        .limits
        .as_mut()
        .ok_or("administrator limits missing")?
        .max_vcpu = 1;
    let mut current_config_value = serde_json::to_value(current_config)?;
    strip_null_configuration_fields(&mut current_config_value);
    fs::write(
        &scenario.config_path,
        serde_yaml_ng::to_string(&current_config_value)?,
    )?;
    let endpoint = scenario.aws.endpoint.clone();
    scenario.restart(&endpoint).await?;
    let current_limit_report = tokio::process::Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(["explain", "--config"])
        .arg(&scenario.config_path)
        .args(["--snapshot", snapshot_id])
        .output()
        .await?;
    if !current_limit_report.status.success() {
        return Err(format!(
            "snapshot explanation with current limits failed: {}",
            String::from_utf8_lossy(&current_limit_report.stderr)
        )
        .into());
    }
    let current_limit_report: Value = serde_json::from_slice(&current_limit_report.stdout)?;
    if current_limit_report["normalized_requirements"]["administrator_max_vcpu"] != 1
        || current_limit_report["candidates"]
            .as_array()
            .is_some_and(|candidates| {
                candidates
                    .iter()
                    .any(|candidate| candidate["eligible"] == true)
            })
    {
        return Err(format!(
            "retained snapshot explanation did not apply current hard limits: {current_limit_report}"
        )
        .into());
    }
    Ok(())
}
