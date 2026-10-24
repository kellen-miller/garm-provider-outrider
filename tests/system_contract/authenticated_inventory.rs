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
