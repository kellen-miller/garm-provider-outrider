use super::*;

#[tokio::test]
async fn inventory_requires_auth_and_reports_failed_native_collection_without_mutation()
-> Result<(), Box<dyn std::error::Error>> {
    let scenario = SystemScenario::start().await?;
    let database = scenario.fixture.directory.join("journal.sqlite3");
    let before = explain_mutation_counts(&database)?;
    let unauthorized = reqwest::Client::new()
        .post(format!("http://{}/v1/inventory", scenario.address))
        .send()
        .await?;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    // This fixture exposes a deliberately non-native `i-synthetic-001` ID.
    // It must fail the entire observation, never return a clean empty report.
    let authorized = reqwest::Client::new()
        .post(format!("http://{}/v1/inventory", scenario.address))
        .bearer_auth(DAEMON_TOKEN)
        .send()
        .await?;
    assert_eq!(authorized.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        authorized.json::<Value>().await?,
        json!({"error":"native inventory is incomplete or unavailable"})
    );
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(["inventory", "--config"])
        .arg(&scenario.config_path)
        .output()
        .await?;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains(DAEMON_TOKEN));
    assert_eq!(before, explain_mutation_counts(&database)?);
    assert!(!scenario.lifecycle_log.exists());
    Ok(())
}

#[tokio::test]
async fn inventory_cli_returns_versioned_projection_through_read_only_sdk_calls()
-> Result<(), Box<dyn std::error::Error>> {
    let scenario = SystemScenario::start().await?;
    scenario.aws.native_visible.store(false, Ordering::Relaxed);
    let database = scenario.fixture.directory.join("journal.sqlite3");
    let before = explain_mutation_counts(&database)?;
    let request_count = scenario
        .aws
        .request_trace
        .lock()
        .map_err(|_| "fixture request lock")?
        .len();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(["inventory", "--config"])
        .arg(&scenario.config_path)
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: outrider::allocation::InventoryReport = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report.schema_version, 1);
    assert_eq!(report.controller_id, CONTROLLER_ID);
    assert!(report.journal_before.active_allocations.is_empty());
    assert!(report.journal_after.active_allocations.is_empty());
    assert_eq!(
        report.journal_before.dispatch_fingerprint,
        report.journal_after.dispatch_fingerprint
    );
    // The fixture deliberately connects both a primary and an alias catalog.
    assert_eq!(
        report
            .catalogs
            .iter()
            .map(|catalog| catalog.catalog_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["aws-east", "aws-east-duplicate"])
    );
    assert!(report.catalogs.iter().all(|catalog| {
        catalog.collections.len() == 3
            && catalog
                .collections
                .iter()
                .all(|collection| collection.resources.is_empty())
    }));
    let requests = scenario
        .aws
        .request_trace
        .lock()
        .map_err(|_| "fixture request lock")?;
    assert_eq!(requests[request_count..].len(), 6);
    assert!(requests[request_count..].iter().all(|request| {
        ["DescribeInstances", "DescribeVolumes", "DescribeAddresses"]
            .iter()
            .any(|operation| request.contains(operation))
    }));
    assert_eq!(before, explain_mutation_counts(&database)?);
    assert!(!scenario.lifecycle_log.exists());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(DAEMON_TOKEN));
    Ok(())
}
