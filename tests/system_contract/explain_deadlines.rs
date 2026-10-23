use super::*;

#[tokio::test]
async fn explain_remote_bounds_chunked_body_and_uses_analysis_deadline()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = std::env::temp_dir().join(format!(
        "outrider-explain-transport-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    fs::create_dir(&fixture)?;
    let token_path = fixture.join("daemon.token");
    fs::write(&token_path, "explanation-transport-token")?;
    let central_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/central.yaml");
    let mut config = ConfigPatch::from_yaml(&fs::read_to_string(central_path)?)?;
    let control = config.control.as_mut().ok_or("control settings missing")?;
    control.auth_token_file = token_path.display().to_string();
    config.ingress = Some(outrider::config::IngressSettings {
        github_api_origin: "http://127.0.0.1".to_owned(),
        github_token_file: "unused".to_owned(),
        garm_api_origin: "http://127.0.0.1".to_owned(),
        garm_token_file: "unused".to_owned(),
        controller_id: "transport-test".to_owned(),
        provider_name: "transport-test".to_owned(),
        pool_flavor: "linux".to_owned(),
        pool_image: "linux".to_owned(),
        runner_prefix: "runner".to_owned(),
        max_dynamic_pools: 1,
        max_runners_per_pool: 1,
        allocation_timeout_seconds: 2,
        bootstrap_timeout_minutes: 1,
        timeout_margin_seconds: 1,
        pool_ttl_seconds: 60,
        webhook_max_bytes: 1024,
        repositories: BTreeMap::new(),
    });

    let listener = TokioTcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    config
        .control
        .as_mut()
        .ok_or("control settings missing")?
        .listen_address = address.to_string();
    let server = tokio::spawn(explanation_response_server(
        listener,
        ExplanationServerResponse::OversizedChunked,
    ));
    let oversized = outrider::daemon::explain_remote(
        &config,
        outrider::allocation::ExplainRequest {
            profile: Some("linux-large".to_owned()),
            snapshot_id: None,
        },
    )
    .await;
    if oversized.is_ok()
        || !oversized
            .err()
            .is_some_and(|error| error.to_string().contains("size limit"))
    {
        return Err("chunked explanation response was not rejected at the byte limit".into());
    }
    server.await??;

    let listener = TokioTcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    config
        .control
        .as_mut()
        .ok_or("control settings missing")?
        .listen_address = address.to_string();
    let server = tokio::spawn(explanation_response_server(
        listener,
        ExplanationServerResponse::Delayed,
    ));
    let started = tokio::time::Instant::now();
    let delayed = outrider::daemon::explain_remote(
        &config,
        outrider::allocation::ExplainRequest {
            profile: Some("linux-large".to_owned()),
            snapshot_id: None,
        },
    )
    .await;
    if delayed.is_ok() || started.elapsed() >= Duration::from_secs(2) {
        return Err("explanation request did not honor its shared analysis deadline".into());
    }
    server.abort();
    fs::remove_dir_all(fixture)?;
    Ok(())
}
