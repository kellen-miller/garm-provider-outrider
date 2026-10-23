use super::*;

#[tokio::test]
async fn central_effective_price_changes_cross_cloud_create_winner_and_explains_provenance()
-> Result<(), Box<dyn std::error::Error>> {
    let mut scenario = CrossCloudScenario::start().await?;
    let client = reqwest::Client::new();
    let baseline_response = client
        .post(format!("http://{}/v1/explain", scenario.address))
        .bearer_auth(DAEMON_TOKEN)
        .json(&json!({"profile":"linux-large"}))
        .send()
        .await?;
    if !baseline_response.status().is_success() {
        return Err(format!("baseline Explain failed: {}", baseline_response.status()).into());
    }
    let baseline: Value = baseline_response.json().await?;
    let candidates = baseline["candidates"]
        .as_array()
        .ok_or("baseline Explain omitted candidates")?;
    let baseline_winner = candidates
        .iter()
        .find(|candidate| candidate["eligible"] == true)
        .ok_or("baseline Explain had no eligible winner")?;
    let baseline_winner_cloud = baseline_winner["financial_price_scope"]["cloud"]
        .as_str()
        .ok_or("baseline winner omitted its cloud scope")?;
    let negotiated = candidates
        .iter()
        .find(|candidate| {
            candidate["eligible"] == true
                && candidate["financial_price_scope"]["cloud"] == "azure"
                && baseline_winner_cloud != "azure"
        })
        .ok_or("baseline lacked an eligible Azure competitor to the public-rate winner")?;
    let negotiated_scope: outrider::allocation::FinancialPriceScope =
        serde_json::from_value(negotiated["financial_price_scope"].clone())?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut effective_prices = Vec::new();
    for (component, field) in [
        (EffectivePriceComponent::Compute, "compute"),
        (EffectivePriceComponent::Disk, "disk"),
        (EffectivePriceComponent::Licensing, "licensing"),
        (EffectivePriceComponent::PublicAddress, "public_address"),
    ] {
        if negotiated["price"][field]["included_in_compute"] == true {
            continue;
        }
        effective_prices.push(EffectivePrice {
            id: format!("negotiated-{}-{field}", negotiated_scope.cloud),
            scope: negotiated_scope.clone(),
            component,
            currency: "USD".to_owned(),
            hourly_rate: if component == EffectivePriceComponent::Compute {
                rust_decimal::Decimal::new(1, 2)
            } else {
                rust_decimal::Decimal::ZERO
            },
            source: "fixture vendor agreement".to_owned(),
            reference: format!("{}-agreement-section-4", negotiated_scope.cloud),
            observed_at_unix_seconds: now.saturating_sub(1),
            valid_from_unix_seconds: now.saturating_sub(1),
            valid_until_unix_seconds: now + 3600,
        });
    }
    if effective_prices.is_empty() {
        return Err("negotiated offer had no overridable components".into());
    }
    let negotiated_cloud = negotiated_scope.cloud.clone();
    let negotiated_identity = negotiated["identity"]
        .as_str()
        .ok_or("negotiated candidate identity missing")?
        .to_owned();
    scenario.config.effective_prices = Some(effective_prices);
    let reloaded_config = serialize_reload_and_validate_central_config(&scenario.config)?;
    if reloaded_config
        .effective_prices
        .as_ref()
        .and_then(|prices| prices.first())
        .is_none_or(|price| price.scope != negotiated_scope)
    {
        return Err("Explain scope changed during central YAML round trip".into());
    }
    scenario.restart_with_config(reloaded_config).await?;
    let snapshot_id = scenario.send_signed_event(ROUTING_LABEL, 281).await?;
    let journal_path = scenario.fixture.directory.join("journal.sqlite3");
    let before_explain = explain_mutation_counts(&journal_path)?;
    let child_logs_before = scenario
        .lifecycle_logs
        .values()
        .map(|path| lifecycle_commands(path))
        .collect::<io::Result<Vec<_>>>()?;
    let response = client
        .post(format!("http://{}/v1/explain", scenario.address))
        .bearer_auth(DAEMON_TOKEN)
        .json(&json!({"snapshot_id":snapshot_id}))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(format!("negotiated Explain failed: {}", response.status()).into());
    }
    let report: Value = response.json().await?;
    let explained = report["candidates"]
        .as_array()
        .ok_or("negotiated Explain omitted candidates")?;
    let explained_target = explained
        .iter()
        .find(|candidate| candidate["identity"] == negotiated_identity)
        .ok_or("negotiated Explain omitted the exact scoped offer")?;
    let compute_override = &explained_target["price"]["compute"]["effective_price_override"];
    if explained_target["financial_price_scope"] != negotiated["financial_price_scope"]
        || compute_override["id"] != format!("negotiated-{negotiated_cloud}-compute")
        || compute_override["reference"] != format!("{negotiated_cloud}-agreement-section-4")
        || compute_override["original_hourly_rate"] != "0.20"
        || compute_override["original_source"] != "Azure Retail Prices API meter vm"
        || compute_override["original_observed_at_unix_seconds"]
            .as_u64()
            .is_none_or(|observed| {
                observed > report["analyzed_at_unix_seconds"].as_u64().unwrap_or(0)
            })
        || compute_override["original_effective_at_unix_seconds"] != 1767225600_u64
        || explained_target["price"]["compute"]["hourly_rate"] != "0.01"
        || explain_mutation_counts(&journal_path)? != before_explain
        || scenario
            .lifecycle_logs
            .values()
            .map(|path| lifecycle_commands(path))
            .collect::<io::Result<Vec<_>>>()?
            != child_logs_before
    {
        return Err(format!(
            "Explain did not preserve the applied contract and public provenance: {report}"
        )
        .into());
    }
    let explained_winner = explained
        .iter()
        .find(|candidate| candidate["eligible"] == true)
        .ok_or("negotiated Explain had no eligible winner")?;
    if explained_winner["identity"] != negotiated_identity {
        return Err(format!(
            "negotiated rate did not change Explain winner to {negotiated_identity}: {}",
            explained_winner["identity"]
        )
        .into());
    }

    let create = scenario
        .create_runner(&snapshot_id, "runner-azure-system")
        .await?;
    if !create.status.success() {
        let allocation_state = scenario
            .journal
            .as_ref()
            .ok_or("cross-cloud journal is closed")?
            .all_allocations()?;
        let child_state = scenario
            .lifecycle_logs
            .iter()
            .map(|(cloud, path)| Ok::<_, io::Error>((cloud, lifecycle_commands(path)?)))
            .collect::<io::Result<Vec<_>>>()?;
        let azure_requests = scenario
            .azure
            .state
            .requests
            .lock()
            .map_err(|_| "Azure request lock poisoned")?
            .clone();
        return Err(format!(
            "negotiated-price fixture Create failed: {}; allocations={allocation_state:?}; child_state={child_state:?}; azure_requests={azure_requests:?}",
            String::from_utf8_lossy(&create.stderr),
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
        .find(|allocation| allocation.runner_name == "runner-azure-system")
        .and_then(|allocation| allocation.selected_offer_json.as_deref())
        .map(serde_json::from_str)
        .transpose()?
        .ok_or("Create did not persist its negotiated selection")?;
    if selected_offer["candidate"]["identity"] != negotiated_identity
        || selected_offer["candidate"]["cloud"] != negotiated_cloud
    {
        return Err(format!(
            "Create did not switch from {baseline_winner_cloud} to negotiated cloud {negotiated_cloud}: {selected_offer}"
        )
        .into());
    }

    assert_effective_price_gates_remain_authoritative(&mut scenario, &client, &negotiated_identity)
        .await?;
    assert_included_component_contract_rejects_routes(&mut scenario, &client, candidates).await?;
    Ok(())
}

pub(super) async fn assert_included_component_contract_rejects_routes(
    scenario: &mut CrossCloudScenario,
    client: &reqwest::Client,
    baseline_candidates: &[Value],
) -> Result<(), Box<dyn std::error::Error>> {
    scenario
        .azure
        .state
        .quote_api_failure
        .store(false, Ordering::Relaxed);
    scenario
        .azure
        .state
        .invalid_image_metadata
        .store(false, Ordering::Relaxed);
    let gcp_included = baseline_candidates
        .iter()
        .find(|candidate| {
            candidate["financial_price_scope"]["cloud"] == "gcp"
                && candidate["price"]["public_address"]["included_in_compute"] == true
        })
        .ok_or("baseline had no GCP included public-address component")?;
    let scope = serde_json::from_value(gcp_included["financial_price_scope"].clone())?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    scenario
        .config
        .effective_prices
        .as_mut()
        .ok_or("negotiated effective-price contract missing")?
        .push(EffectivePrice {
            id: "invalid-gcp-included-public-address".to_owned(),
            scope,
            component: EffectivePriceComponent::PublicAddress,
            currency: "USD".to_owned(),
            hourly_rate: rust_decimal::Decimal::new(1, 2),
            source: "fixture vendor agreement".to_owned(),
            reference: "included-address-section".to_owned(),
            observed_at_unix_seconds: now,
            valid_from_unix_seconds: now,
            valid_until_unix_seconds: now + 3600,
        });
    scenario
        .restart_with_config(scenario.config.clone())
        .await?;
    let snapshot_id = scenario
        .send_signed_event(&format!("{ROUTING_LABEL}:memory=24gib"), 285)
        .await?;
    let child_logs_before = scenario
        .lifecycle_logs
        .values()
        .map(|path| lifecycle_commands(path))
        .collect::<io::Result<Vec<_>>>()?;
    let (explain_status, explain_report) =
        explain_cross_cloud_snapshot(client, scenario, &snapshot_id).await?;
    let create = scenario
        .create_runner(&snapshot_id, "runner-invalid-effective-price")
        .await?;
    let child_logs_after = scenario
        .lifecycle_logs
        .values()
        .map(|path| lifecycle_commands(path))
        .collect::<io::Result<Vec<_>>>()?;
    if explain_report["policy_error"].is_null()
        || create.status.success()
        || child_logs_before != child_logs_after
    {
        return Err(format!(
            "active included-component contract did not reject Explain/Create without child calls: explain={explain_status}; report={explain_report}; create={}; before={child_logs_before:?}; after={child_logs_after:?}",
            create.status
        )
        .into());
    }
    Ok(())
}

pub(super) async fn assert_effective_price_gates_remain_authoritative(
    scenario: &mut CrossCloudScenario,
    client: &reqwest::Client,
    negotiated_identity: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let administrator_limits = scenario
        .config
        .limits
        .as_mut()
        .ok_or("cross-cloud administrator limits missing for cost ceiling proof")?;
    administrator_limits.max_total_cost = Some(rust_decimal::Decimal::from(100));
    let economy_policy = scenario
        .config
        .policies
        .as_mut()
        .and_then(|policies| policies.get_mut("economy"))
        .ok_or("cross-cloud economy policy missing for live quote proof")?;
    let outrider::policy::Policy::Economy {
        max_price_age_seconds,
    } = &mut economy_policy.strategy
    else {
        return Err("cross-cloud economy policy changed strategy".into());
    };
    *max_price_age_seconds = 0;
    let negotiated_compute = scenario
        .config
        .effective_prices
        .as_mut()
        .and_then(|prices| {
            prices
                .iter_mut()
                .find(|price| price.component == EffectivePriceComponent::Compute)
        })
        .ok_or("negotiated compute contract missing for cost ceiling proof")?;
    negotiated_compute.hourly_rate = rust_decimal::Decimal::from(1_000_000);
    scenario
        .restart_with_config(scenario.config.clone())
        .await?;
    let capped_label = format!("{ROUTING_LABEL}:vcpu=8");
    let capped_snapshot = scenario
        .send_signed_event(&capped_label, 282)
        .await
        .map_err(|error| format!("cost ceiling snapshot creation failed: {error}"))?;
    let (capped_status, capped_report) =
        explain_cross_cloud_snapshot(client, scenario, &capped_snapshot).await?;
    if !capped_status.is_success() {
        return Err(format!("cost-capped Explain failed: {capped_status}").into());
    }
    let capped_candidate = capped_report["candidates"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|candidate| candidate["identity"] == negotiated_identity)
        })
        .ok_or("cost-capped Explain omitted the exact negotiated offer")?;
    if capped_candidate["eligible"] != false
        || capped_candidate["reason"] != "above maximum total cost"
        || capped_candidate["price"]["compute"]["hourly_rate"] != "1000000"
        || capped_candidate["price"]["compute"]["effective_price_override"]["id"]
            != "negotiated-azure-compute"
    {
        return Err(format!(
            "active negotiated rate bypassed the hard cost ceiling: {capped_candidate}"
        )
        .into());
    }

    scenario
        .restart_with_config(scenario.config.clone())
        .await?;
    scenario
        .azure
        .state
        .quote_api_failure
        .store(true, Ordering::Relaxed);
    let failed_quote_label = format!("{ROUTING_LABEL}:memory=32gib");
    let failed_quote_snapshot = scenario
        .send_signed_event(&failed_quote_label, 283)
        .await
        .map_err(|error| format!("quote-failure snapshot creation failed: {error}"))?;
    let (failed_quote_status, failed_quote_report) =
        explain_cross_cloud_snapshot(client, scenario, &failed_quote_snapshot).await?;
    let azure_pricing_request = scenario
        .azure
        .state
        .requests
        .lock()
        .map_err(|_| "Azure request lock poisoned")?
        .iter()
        .any(|(_, path)| path.contains("/api/retail/prices"));
    let azure_pricing_503 = scenario
        .azure
        .state
        .quote_api_failures
        .load(Ordering::Relaxed)
        > 0;
    let failed_quote_candidate = failed_quote_report["candidates"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|candidate| candidate["identity"] == negotiated_identity)
        });
    if !failed_quote_status.is_success()
        || !azure_pricing_request
        || !azure_pricing_503
        || failed_quote_candidate.is_none_or(|candidate| {
            candidate["eligible"] == true
                || !candidate["price"].is_null()
                || !failed_quote_report["quote_missing"]
                    .as_bool()
                    .unwrap_or(false)
        })
    {
        return Err(format!(
            "active negotiated rate rescued unavailable Azure catalog pricing: status={failed_quote_status}; report={failed_quote_report}; azure_503={azure_pricing_503}; azure_request={azure_pricing_request}"
        )
        .into());
    }

    scenario
        .azure
        .state
        .quote_api_failure
        .store(false, Ordering::Relaxed);
    scenario
        .restart_with_config(scenario.config.clone())
        .await?;
    let invalid_image_response_count = scenario
        .azure
        .state
        .invalid_image_responses
        .load(Ordering::Relaxed);
    scenario
        .azure
        .state
        .invalid_image_metadata
        .store(true, Ordering::Relaxed);
    let invalid_image_label = format!("{ROUTING_LABEL}:image=ubuntu-24");
    let invalid_image_snapshot = scenario
        .send_signed_event(&invalid_image_label, 284)
        .await?;
    let (invalid_image_status, invalid_image_report) =
        explain_cross_cloud_snapshot(client, scenario, &invalid_image_snapshot).await?;
    if invalid_image_status.is_success()
        || scenario
            .azure
            .state
            .invalid_image_responses
            .load(Ordering::Relaxed)
            <= invalid_image_response_count
        || invalid_image_report["candidates"]
            .as_array()
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|candidate| candidate["identity"] == negotiated_identity)
            })
    {
        return Err(format!(
            "active negotiated rate rescued invalid Azure image metadata: {invalid_image_report}"
        )
        .into());
    }
    Ok(())
}
