use super::*;

#[tokio::test]
async fn cli_explain_uses_authenticated_daemon_and_read_only_sdk_analysis()
-> Result<(), Box<dyn std::error::Error>> {
    let mut scenario = SystemScenario::start().await?;
    let mut central = ConfigPatch::from_yaml(&fs::read_to_string(&scenario.config_path)?)?;
    central.history = Some(outrider::config::HistorySettings {
        window_seconds: 3600,
        sampling_interval_seconds: 60,
        maximum_gap_seconds: 180,
    });
    let policies = central
        .policies
        .as_mut()
        .ok_or("central policies missing")?;
    policies.insert(
        "explain-weighted".to_owned(),
        outrider::policy::PolicyDefinition {
            strategy: outrider::policy::Policy::Weighted(outrider::policy::WeightedPolicy {
                cost_weight: rust_decimal::Decimal::ZERO,
                failure_weight: rust_decimal::Decimal::ZERO,
                interruption_weight: rust_decimal::Decimal::ONE,
                latency_weight: rust_decimal::Decimal::ONE,
                cost_scale: rust_decimal::Decimal::ONE,
                interruption_scale: rust_decimal::Decimal::ONE,
                latency_scale: rust_decimal::Decimal::ONE,
                min_samples: 1,
                max_observation_age_seconds: 600,
                max_price_age_seconds: 600,
            }),
            missing_data: outrider::policy::MissingDataBehavior::Fallback {
                policy: "explain-cost".to_owned(),
            },
        },
    );
    policies.insert(
        "explain-cost".to_owned(),
        outrider::policy::PolicyDefinition {
            strategy: outrider::policy::Policy::Weighted(outrider::policy::WeightedPolicy {
                cost_weight: rust_decimal::Decimal::ONE,
                failure_weight: rust_decimal::Decimal::ZERO,
                interruption_weight: rust_decimal::Decimal::ZERO,
                latency_weight: rust_decimal::Decimal::ZERO,
                cost_scale: rust_decimal::Decimal::ONE,
                interruption_scale: rust_decimal::Decimal::ONE,
                latency_scale: rust_decimal::Decimal::ONE,
                min_samples: 1,
                max_observation_age_seconds: 600,
                max_price_age_seconds: 600,
            }),
            missing_data: outrider::policy::MissingDataBehavior::Fallback {
                policy: "economy".to_owned(),
            },
        },
    );
    central.default_policy = Some("explain-weighted".to_owned());
    let mut central_value = serde_json::to_value(central)?;
    strip_null_configuration_fields(&mut central_value);
    fs::write(
        &scenario.config_path,
        serde_yaml_ng::to_string(&central_value)?,
    )?;
    let endpoint = scenario.aws.endpoint.clone();
    scenario.restart(&endpoint).await?;
    scenario.send_signed_event().await?;
    let pool = scenario.pool()?;
    let snapshot_id = pool["extra_specs"]["snapshot_id"]
        .as_str()
        .ok_or("retained snapshot ID missing")?;
    let before = explain_mutation_counts(&scenario.fixture.directory.join("journal.sqlite3"))?;
    let api_requests_before = scenario.dependencies.requests().len();

    let unauthorized = reqwest::Client::new()
        .post(format!("http://{}/v1/explain", scenario.address))
        .json(&json!({"snapshot_id":snapshot_id}))
        .send()
        .await?;
    if unauthorized.status() != StatusCode::UNAUTHORIZED {
        return Err("explanation route accepted an unauthenticated request".into());
    }

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(["explain", "--config"])
        .arg(&scenario.config_path)
        .args(["--snapshot", snapshot_id])
        .output()
        .await?;
    if !output.status.success() {
        return Err(format!(
            "CLI explanation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let report: Value = serde_json::from_slice(&output.stdout)?;
    if report["input_source"] != "retained_repository_job_snapshot"
        || report["snapshot"]["id"] != snapshot_id
        || report["candidate_count"].as_u64().unwrap_or_default() == 0
        || report["applied_policy"] != "explain-cost"
        || report["fallback_from"] != "explain-weighted"
        || scenario.aws.compute_quote_requests.load(Ordering::Relaxed) == 0
    {
        return Err(
            format!("explanation omitted retained source or SDK candidates: {report}").into(),
        );
    }
    let unknown_reasons = report["candidates"][0]["history"]["unknown_reasons"]
        .as_array()
        .ok_or("history unknown reasons missing")?;
    if !unknown_reasons.iter().any(|reason| {
        reason
            .as_str()
            .is_some_and(|value| value.contains("no measured create latency"))
    }) || !unknown_reasons.iter().any(|reason| {
        reason
            .as_str()
            .is_some_and(|value| value.contains("no verified running interval"))
    }) {
        return Err(format!("Explain lost weighted history diagnostics: {report}").into());
    }
    let first_eligible = report["candidates"]
        .as_array()
        .and_then(|candidates| {
            candidates
                .iter()
                .find(|candidate| candidate["eligible"] == true)
        })
        .ok_or("explanation did not report an eligible candidate")?;
    if first_eligible["score_terms"]
        .as_array()
        .is_none_or(Vec::is_empty)
    {
        return Err("actual scoring path omitted typed score terms".into());
    }
    let first_scope: outrider::allocation::FinancialPriceScope =
        serde_json::from_value(report["candidates"][0]["financial_price_scope"].clone())?;
    let scope_json = serde_json::to_value(&first_scope)?;
    let scope_yaml = serde_yaml_ng::to_string(&scope_json)?;
    let roundtripped_scope: Value = serde_yaml_ng::from_str(&scope_yaml)?;
    if roundtripped_scope != scope_json || scope_yaml.contains("null") {
        return Err("financial price scope did not round-trip without null values".into());
    }
    let current_profile = tokio::process::Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(["explain", "--config"])
        .arg(&scenario.config_path)
        .args(["--profile", "linux-large"])
        .output()
        .await?;
    if !current_profile.status.success() {
        return Err(format!(
            "central-profile explanation failed: {}",
            String::from_utf8_lossy(&current_profile.stderr)
        )
        .into());
    }
    let current_report: Value = serde_json::from_slice(&current_profile.stdout)?;
    if current_report["input_source"] != "current_central_profile"
        || current_report["candidate_count"]
            .as_u64()
            .unwrap_or_default()
            == 0
    {
        return Err("central-profile explanation omitted its source or candidates".into());
    }
    let missing_profile = tokio::process::Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(["explain", "--config"])
        .arg(&scenario.config_path)
        .args(["--profile", "missing-profile"])
        .output()
        .await?;
    if missing_profile.status.success() {
        return Err("explanation silently substituted a missing profile".into());
    }
    let serialized = serde_json::to_string(&report)?;
    if serialized.contains(DAEMON_TOKEN)
        || serialized.contains(scenario.config_path.to_string_lossy().as_ref())
        || serialized.contains("secret-test-only-not-a-real-key")
    {
        return Err("explanation leaked daemon configuration or credentials".into());
    }
    if explain_mutation_counts(&scenario.fixture.directory.join("journal.sqlite3"))? != before
        || scenario.dependencies.requests().len() != api_requests_before
        || scenario.lifecycle_log.exists()
            && !lifecycle_commands(&scenario.lifecycle_log)?.is_empty()
    {
        return Err(
            "explanation changed allocation state or invoked lifecycle dependencies".into(),
        );
    }
    let create = scenario.create_runner().await?;
    if !create.status.success() {
        return Err(format!(
            "fixture Create failed after explanation: {}",
            String::from_utf8_lossy(&create.stderr)
        )
        .into());
    }
    let selected_offer_json: String =
        Connection::open(scenario.fixture.directory.join("journal.sqlite3"))?.query_row(
            "SELECT selected_offer_json FROM allocations WHERE controller_id=?1 ORDER BY created_at DESC LIMIT 1",
            rusqlite::params![CONTROLLER_ID],
            |row| row.get(0),
        )?;
    let selected_offer: Value = serde_json::from_str(&selected_offer_json)?;
    let explained_winner = report["candidates"]
        .as_array()
        .and_then(|candidates| {
            candidates
                .iter()
                .find(|candidate| candidate["eligible"] == true)
        })
        .ok_or("explanation did not report an eligible candidate")?;
    if selected_offer["candidate"]["identity"] != explained_winner["identity"] {
        return Err(format!(
            "Create winner {} differs from Explain order {}",
            selected_offer["candidate"]["identity"], explained_winner["identity"]
        )
        .into());
    }

    assert_explanation_retention_policy_and_limit_diagnostics(&mut scenario, snapshot_id).await?;
    Ok(())
}

#[tokio::test]
async fn cli_explain_matches_cross_cloud_create_order_without_mutations()
-> Result<(), Box<dyn std::error::Error>> {
    let scenario = CrossCloudScenario::start().await?;
    let snapshot_id = scenario.send_signed_event(ROUTING_LABEL, 201).await?;
    let journal_path = scenario.fixture.directory.join("journal.sqlite3");
    let before = explain_mutation_counts(&journal_path)?;
    let dependency_requests = scenario.dependencies.requests().len();
    let child_logs = scenario
        .lifecycle_logs
        .values()
        .map(|path| lifecycle_commands(path))
        .collect::<io::Result<Vec<_>>>()?;

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(["explain", "--config"])
        .arg(&scenario.config_path)
        .args(["--snapshot", &snapshot_id])
        .output()
        .await?;
    if !output.status.success() {
        return Err(format!(
            "cross-cloud CLI explanation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let report: Value = serde_json::from_slice(&output.stdout)?;
    let candidates = report["candidates"]
        .as_array()
        .ok_or("cross-cloud explanation omitted candidates")?;
    if report["candidate_count"].as_u64().unwrap_or_default() < 2
        || report["queried_catalog_count"].as_u64().unwrap_or_default() != 3
        || report["observed_exclusion_count"]
            .as_u64()
            .unwrap_or_default()
            == 0
        || candidates
            .iter()
            .filter(|candidate| candidate["eligible"] == true)
            .count()
            < 2
    {
        return Err(format!(
            "cross-cloud explanation did not compare multiple eligible offers: {report}"
        )
        .into());
    }
    let queried_catalogs = report["queried_catalogs"]
        .as_array()
        .ok_or("cross-cloud query scopes missing")?;
    for (cloud, location) in [
        ("aws", "east-ci"),
        ("azure", "east-ci"),
        ("gcp", "us-central1"),
    ] {
        let native_location = match cloud {
            "aws" => "us-east-1",
            "azure" => "eastus",
            "gcp" => "us-central1-a",
            _ => unreachable!(),
        };
        if !queried_catalogs.iter().any(|scope| {
            scope["cloud"] == cloud
                && scope["locations"] == json!([location])
                && scope["native_locations"] == json!([native_location])
                && scope["queried"] == true
        }) {
            return Err(format!(
                "cross-cloud report did not retain actual {cloud} query scope: {queried_catalogs:?}"
            )
            .into());
        }
    }
    if explain_mutation_counts(&journal_path)? != before
        || scenario.dependencies.requests().len() != dependency_requests
        || scenario
            .lifecycle_logs
            .values()
            .map(|path| lifecycle_commands(path))
            .collect::<io::Result<Vec<_>>>()?
            != child_logs
    {
        return Err("cross-cloud explanation changed durable or child state".into());
    }

    let create = scenario
        .create_runner(&snapshot_id, "runner-old-image")
        .await?;
    if !create.status.success() {
        return Err(format!(
            "cross-cloud fixture Create failed: {}",
            String::from_utf8_lossy(&create.stderr)
        )
        .into());
    }
    let allocations = scenario
        .journal
        .as_ref()
        .ok_or("cross-cloud journal is closed")?
        .all_allocations()?;
    let selected_offer: Value = allocations
        .iter()
        .find(|allocation| allocation.runner_name == "runner-old-image")
        .and_then(|allocation| allocation.selected_offer_json.as_deref())
        .map(serde_json::from_str)
        .transpose()?
        .ok_or("Create did not persist its selected offer")?;
    let explained_winner = candidates
        .iter()
        .find(|candidate| candidate["eligible"] == true)
        .ok_or("explanation did not report an eligible winner")?;
    if selected_offer["candidate"]["identity"] != explained_winner["identity"] {
        return Err(format!(
            "Create winner {} differs from Explain winner {}",
            selected_offer["candidate"]["identity"], explained_winner["identity"]
        )
        .into());
    }
    Ok(())
}

pub(super) async fn explain_cross_cloud_snapshot(
    client: &reqwest::Client,
    scenario: &CrossCloudScenario,
    snapshot_id: &str,
) -> Result<(reqwest::StatusCode, Value), Box<dyn std::error::Error>> {
    let response = client
        .post(format!("http://{}/v1/explain", scenario.address))
        .bearer_auth(DAEMON_TOKEN)
        .json(&json!({"snapshot_id":snapshot_id}))
        .send()
        .await?;
    let status = response.status();
    let body = response.json().await.unwrap_or(Value::Null);
    Ok((status, body))
}
