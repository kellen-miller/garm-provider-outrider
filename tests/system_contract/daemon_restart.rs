use super::*;

#[tokio::test]
async fn restart_idempotent_end_to_end() -> Result<(), Box<dyn std::error::Error>> {
    let mut scenario = SystemScenario::start().await?;
    scenario.send_signed_event().await?;
    let create = scenario.create_runner().await?;
    if !create.status.success() {
        let daemon_stderr =
            fs::read_to_string(scenario.lifecycle_log.with_extension("daemon.stderr"))
                .unwrap_or_default();
        let lifecycle = fs::read_to_string(&scenario.lifecycle_log).unwrap_or_default();
        return Err(format!("GARM CreateInstance failed: {}; quotes={}; lifecycle={lifecycle}; daemon={daemon_stderr}", String::from_utf8_lossy(&create.stderr), scenario.aws.compute_quote_requests.load(Ordering::Relaxed)).into());
    }
    if scenario.aws.compute_quote_requests.load(Ordering::Relaxed) == 0 {
        return Err("AWS SDK did not query the concrete HTTP fixture".into());
    }
    let pool = scenario.pool()?;
    let snapshot_id = pool["extra_specs"]["snapshot_id"]
        .as_str()
        .ok_or("pool snapshot ID missing")?;
    if snapshot_id.is_empty() {
        return Err("pool omitted snapshot ID".into());
    }
    scenario
        .aws
        .native_inspection_mode
        .store(0, Ordering::Relaxed);
    let endpoint = scenario.aws.endpoint.clone();
    scenario.restart(&endpoint).await?;
    let first_cleanup = scenario.remove_all().await?;
    if !first_cleanup.status.success() {
        return Err(format!(
            "RemoveAll after restart failed: {}",
            String::from_utf8_lossy(&first_cleanup.stderr)
        )
        .into());
    }
    let second_cleanup = scenario.remove_all().await?;
    if !second_cleanup.status.success() {
        return Err(format!(
            "repeated RemoveAll failed: {}",
            String::from_utf8_lossy(&second_cleanup.stderr)
        )
        .into());
    }
    let commands = lifecycle_commands(&scenario.lifecycle_log)?;
    if commands
        .iter()
        .filter(|command| command.as_str() == "CreateInstance")
        .count()
        != 1
        || commands
            .iter()
            .filter(|command| command.as_str() == "DeleteInstance")
            .count()
            != 1
    {
        return Err(
            format!("unexpected pinned-child lifecycle across restart: {commands:?}").into(),
        );
    }
    scenario.complete_run_and_wait_for_pool_drain().await?;
    scenario.daemon.kill()?;
    scenario.daemon.wait()?;
    let journal = scenario.fixture.journal();
    let snapshot = journal
        .pool_snapshot(101, ROUTING_LABEL)?
        .ok_or("durable request snapshot missing after restart cleanup")?;
    if snapshot.id != snapshot_id {
        return Err("GARM pool snapshot ID differs from the durable request snapshot".into());
    }
    let allocations = journal.all_allocations()?;
    if allocations.len() != 1 || allocations[0].state != "deleted" {
        return Err(format!(
            "restart cleanup did not leave one deleted allocation: {allocations:?}"
        )
        .into());
    }
    drop(journal);

    let old_provider_files =
        replace_current_provider_files(&scenario.config_path, &scenario.fixture.directory)?;
    for old_file in old_provider_files {
        fs::remove_file(old_file)?;
    }

    scenario.daemon = start_daemon(
        &scenario.config_path,
        &scenario.address,
        &scenario.aws.endpoint,
        &scenario.lifecycle_log,
    )
    .await?;
    let list = provider_lifecycle_process(
        &scenario.config_path,
        "ListInstances",
        Some(&allocations[0].pool_id),
        None,
    )
    .await?;
    if !list.status.success() {
        return Err(format!(
            "List with terminal allocation and removed old child files failed: {}",
            String::from_utf8_lossy(&list.stderr)
        )
        .into());
    }
    let listed: Value = serde_json::from_slice(&list.stdout)?;
    if listed != json!([]) {
        return Err(format!("terminal allocation appeared in List output: {listed}").into());
    }
    let delete = provider_lifecycle_process(
        &scenario.config_path,
        "DeleteInstance",
        Some(&allocations[0].pool_id),
        Some(&allocations[0].runner_name),
    )
    .await?;
    if !delete.status.success() {
        return Err(format!(
            "repeated Delete with removed old child files failed: {}",
            String::from_utf8_lossy(&delete.stderr)
        )
        .into());
    }
    let commands = lifecycle_commands(&scenario.lifecycle_log)?;
    if commands
        .iter()
        .filter(|command| command.as_str() == "DeleteInstance")
        .count()
        != 1
    {
        return Err(format!(
            "repeated Delete invoked a child after terminal cleanup: {commands:?}"
        )
        .into());
    }
    Ok(())
}

#[tokio::test]
async fn active_allocation_requires_its_snapshot_fingerprint()
-> Result<(), Box<dyn std::error::Error>> {
    let mut scenario = SystemScenario::start().await?;
    scenario.send_signed_event().await?;
    let create = scenario.create_runner().await?;
    if !create.status.success() {
        return Err(format!(
            "GARM CreateInstance failed: {}",
            String::from_utf8_lossy(&create.stderr)
        )
        .into());
    }
    scenario.daemon.kill()?;
    scenario.daemon.wait()?;

    let database = scenario.fixture.directory.join("journal.sqlite3");
    let connection = Connection::open(&database)?;
    connection.execute(
        "UPDATE allocations SET catalog_fingerprint='missing-retained-scope'",
        [],
    )?;
    drop(connection);

    let result = start_daemon(
        &scenario.config_path,
        &scenario.address,
        &scenario.aws.endpoint,
        &scenario.lifecycle_log,
    )
    .await;
    let error = result.expect_err("startup must reject an absent live fingerprint");
    if !error
        .to_string()
        .contains("allocation configuration is invalid")
    {
        return Err(format!("unexpected startup error: {error}").into());
    }
    Ok(())
}

#[tokio::test]
async fn live_allocation_requires_historical_files_and_snapshot()
-> Result<(), Box<dyn std::error::Error>> {
    let mut scenario = SystemScenario::start().await?;
    scenario.send_signed_event().await?;
    let create = scenario.create_runner().await?;
    if !create.status.success() {
        return Err(format!(
            "GARM CreateInstance failed: {}",
            String::from_utf8_lossy(&create.stderr)
        )
        .into());
    }
    scenario.daemon.kill()?;
    scenario.daemon.wait()?;

    let database = scenario.fixture.directory.join("journal.sqlite3");
    let journal = scenario.fixture.journal();
    let allocation = journal
        .all_allocations()?
        .into_iter()
        .next()
        .ok_or("live allocation missing")?;
    let snapshot = journal
        .pool_snapshot(101, ROUTING_LABEL)?
        .ok_or("live allocation snapshot missing")?;
    drop(journal);

    // An unresolved allocation normally keeps its pool from draining. Seed the
    // retained, drained-snapshot case to exercise the historical dependency gate.
    let connection = Connection::open(&database)?;
    connection.execute(
        "UPDATE pool_snapshots SET state='deleted' WHERE id=?1",
        [&snapshot.id],
    )?;
    connection.execute(
        "UPDATE allocations SET state='ambiguous' WHERE allocation_id=?1",
        [&allocation.allocation_id],
    )?;
    drop(connection);

    let old_files =
        replace_current_provider_files(&scenario.config_path, &scenario.fixture.directory)?;
    for old_file in old_files {
        fs::remove_file(old_file)?;
    }
    let startup = start_daemon(
        &scenario.config_path,
        &scenario.address,
        &scenario.aws.endpoint,
        &scenario.lifecycle_log,
    )
    .await;
    let error = startup.expect_err("startup must require live historical credentials");
    if !error
        .to_string()
        .contains("allocation catalog is unavailable")
    {
        return Err(format!("unexpected historical dependency error: {error}").into());
    }

    let connection = Connection::open(&database)?;
    connection.execute("DELETE FROM pool_snapshots WHERE id=?1", [&snapshot.id])?;
    drop(connection);
    let startup = start_daemon(
        &scenario.config_path,
        &scenario.address,
        &scenario.aws.endpoint,
        &scenario.lifecycle_log,
    )
    .await;
    let error = startup.expect_err("startup must reject a missing live snapshot");
    if !error
        .to_string()
        .contains("allocation snapshot is missing or unavailable")
    {
        return Err(format!("unexpected missing snapshot error: {error}").into());
    }
    Ok(())
}

#[tokio::test]
async fn drained_pool_reconnects_only_live_catalog_scope() -> Result<(), Box<dyn std::error::Error>>
{
    let mut scenario = SystemScenario::start().await?;
    scenario.send_signed_event().await?;
    let create = scenario.create_runner().await?;
    if !create.status.success() {
        return Err(format!(
            "GARM CreateInstance failed: {}",
            String::from_utf8_lossy(&create.stderr)
        )
        .into());
    }
    scenario.daemon.kill()?;
    scenario.daemon.wait()?;

    let database = scenario.fixture.directory.join("journal.sqlite3");
    let connection = Connection::open(&database)?;
    connection.execute("UPDATE pool_snapshots SET state='deleted'", [])?;
    connection.execute("UPDATE allocations SET state='ambiguous'", [])?;
    drop(connection);

    let old_config = ConfigPatch::from_toml(&fs::read_to_string(&scenario.config_path)?)?;
    let allocation = scenario
        .fixture
        .journal()
        .all_allocations()?
        .into_iter()
        .next()
        .ok_or("live allocation missing")?;
    let retained = scenario
        .fixture
        .journal()
        .pool_snapshot(101, ROUTING_LABEL)?
        .ok_or("retained pool snapshot missing")?;
    let snapshot: outrider::github::SnapshotDocument = serde_json::from_str(&retained.config_json)?;
    let selected_catalog = snapshot
        .central
        .aws_catalog_bindings
        .as_deref()
        .and_then(|catalogs| {
            catalogs.iter().find(|catalog| {
                catalog_configuration_fingerprint(&snapshot.central, "aws", &catalog.id).is_ok_and(
                    |fingerprint| {
                        allocation.catalog_fingerprint.as_deref() == Some(fingerprint.as_str())
                    },
                )
            })
        })
        .ok_or("live allocation fingerprint missing from retained snapshot")?;
    let sibling_binding_id = snapshot
        .central
        .aws_catalog_bindings
        .as_deref()
        .and_then(|catalogs| {
            catalogs
                .iter()
                .find(|catalog| catalog.id != selected_catalog.id)
                .map(|catalog| catalog.provider_binding_id.as_str())
        })
        .ok_or("sibling catalog missing")?;
    let sibling_binding = old_config
        .provider_bindings
        .as_deref()
        .and_then(|bindings| {
            bindings
                .iter()
                .find(|binding| binding.id == sibling_binding_id)
        })
        .ok_or("sibling binding missing from current config")?;
    let sibling_executable = sibling_binding.executable_path.clone();
    let sibling_configuration = sibling_binding.configuration_path.clone();

    let old_provider_files =
        replace_current_provider_files(&scenario.config_path, &scenario.fixture.directory)?;
    for old_file in old_provider_files {
        if old_file != sibling_executable && old_file != sibling_configuration {
            continue;
        }
        fs::remove_file(old_file)?;
    }

    scenario.daemon = start_daemon(
        &scenario.config_path,
        &scenario.address,
        &scenario.aws.endpoint,
        &scenario.lifecycle_log,
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn create_waits_for_a_valid_allocation_longer_than_provider_timeout()
-> Result<(), Box<dyn std::error::Error>> {
    let scenario = SystemScenario::start_with_provider_timeout(false, 1).await?;
    scenario.send_signed_event().await?;
    scenario
        .aws
        .slow_next_request
        .store(true, Ordering::Relaxed);
    let started = std::time::Instant::now();
    let create = scenario.create_runner().await?;
    let elapsed = started.elapsed();
    if !create.status.success() {
        let daemon_stderr =
            fs::read_to_string(scenario.lifecycle_log.with_extension("daemon.stderr"))
                .unwrap_or_default();
        return Err(format!(
            "slow valid GARM CreateInstance failed after {elapsed:?}: {}; quotes={}; daemon={daemon_stderr}",
            String::from_utf8_lossy(&create.stderr),
            scenario.aws.compute_quote_requests.load(Ordering::Relaxed)
        )
        .into());
    }
    if elapsed < Duration::from_millis(1500) {
        return Err(format!("AWS SDK Create delay was not observed: {elapsed:?}").into());
    }

    if scenario.aws.compute_quote_requests.load(Ordering::Relaxed) == 0 {
        return Err("AWS SDK did not query the concrete HTTP fixture".into());
    }
    let created = lifecycle_commands(&scenario.lifecycle_log)?;
    if created
        .iter()
        .filter(|command| command.as_str() == "CreateInstance")
        .count()
        != 1
    {
        return Err(format!("slow Create did not execute exactly once: {created:?}").into());
    }

    let cleanup = scenario.remove_all().await?;
    if !cleanup.status.success() {
        return Err(format!(
            "RemoveAll after slow Create failed: {}",
            String::from_utf8_lossy(&cleanup.stderr)
        )
        .into());
    }
    let cleaned = lifecycle_commands(&scenario.lifecycle_log)?;
    if cleaned
        .iter()
        .filter(|command| command.as_str() == "DeleteInstance")
        .count()
        != 1
    {
        return Err(format!("slow Create cleanup was not exact: {cleaned:?}").into());
    }
    Ok(())
}

#[tokio::test]
async fn repository_registration_labels() -> Result<(), Box<dyn std::error::Error>> {
    let scenario = SystemScenario::start().await?;
    scenario.send_signed_event().await?;
    let create = scenario.create_runner().await?;
    if !create.status.success() {
        return Err(format!(
            "shipping CLI CreateInstance failed: {}",
            String::from_utf8_lossy(&create.stderr)
        )
        .into());
    }
    let pool = scenario.pool()?;
    let tags = pool["tags"]
        .as_array()
        .ok_or("GARM pool tags are missing")?;
    let tag_names = tags
        .iter()
        .map(|tag| tag["name"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    if tag_names != [scenario.routing_label.as_str()] {
        return Err(format!(
            "repository pool labels differ from the sole internal routing token: {tag_names:?}"
        )
        .into());
    }
    let record: Value = serde_json::from_slice(&fs::read(
        scenario.lifecycle_log.with_extension("record.json"),
    )?)?;
    if record["controller"] != CONTROLLER_ID || record["pool"] != pool["id"] {
        return Err(format!("pinned child did not receive GARM ownership IDs: {record}").into());
    }
    let bootstrap_bytes = STANDARD.decode(
        record["raw"]
            .as_str()
            .ok_or("pinned child bootstrap record is missing")?,
    )?;
    let bootstrap: Value = serde_json::from_slice(&bootstrap_bytes)?;
    if bootstrap["repo_url"] != "https://github.com/base-owner/base-repo"
        || bootstrap["labels"] != json!([])
    {
        return Err(format!(
            "pinned child bootstrap changed the repository URL or JIT labels: {bootstrap}"
        )
        .into());
    }
    let requests = scenario.dependencies.requests();
    let github_requests = requests
        .iter()
        .filter(|request| request.path.starts_with("/repos/"))
        .collect::<Vec<_>>();
    if github_requests.is_empty()
        || github_requests.iter().any(|request| {
            request.authorization.as_deref() != Some("Bearer synthetic-github-token")
        })
    {
        return Err("GitHub API requests did not all use the exact fixture bearer token".into());
    }
    let forwarded = requests
        .iter()
        .find(|request| {
            request.method == "POST"
                && request.path == "/webhooks/00000000-0000-0000-0000-000000000901"
        })
        .ok_or("signed event was not forwarded to GARM")?;
    let garm_requests = requests
        .iter()
        .filter(|request| request.path.starts_with("/api/v1/"))
        .collect::<Vec<_>>();
    if garm_requests.is_empty()
        || garm_requests
            .iter()
            .any(|request| request.authorization.as_deref() != Some("Bearer synthetic-garm-token"))
    {
        return Err("GARM API requests did not all use the exact fixture bearer token".into());
    }
    if forwarded.body.as_slice() != include_bytes!("../fixtures/system/workflow-job.json") {
        return Err("forwarded event body changed".into());
    }
    let mut expected_mac = HookMac::new_from_slice(WEBHOOK_SECRET)?;
    expected_mac.update(&forwarded.body);
    let expected_signature = format!(
        "sha256={}",
        hex::encode(expected_mac.finalize().into_bytes())
    );
    if forwarded.signature.as_deref() != Some(expected_signature.as_str()) {
        return Err(
            "forwarded webhook signature does not match the independently computed HMAC".into(),
        );
    }
    let label = Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(["label", "--token", &scenario.routing_label])
        .output()?;
    if !label.status.success()
        || String::from_utf8_lossy(&label.stdout).trim()
            != format!("canonical: {}", scenario.routing_label)
    {
        return Err("label CLI did not preserve canonical repository request token".into());
    }
    Ok(())
}
