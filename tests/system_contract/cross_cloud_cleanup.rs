use super::*;

#[tokio::test]
async fn unsupported_macos_fail_fast() -> Result<(), Box<dyn std::error::Error>> {
    let scenario = SystemScenario::start_with_macos_profile(true).await?;
    scenario.post_signed_event().await?;
    let snapshot = scenario.wait_for_snapshot().await?;
    verify_snapshot(&snapshot, "macos", "arm64")?;
    if snapshot.state != "prepared" || snapshot.garm_pool_id.is_some() {
        return Err(format!(
            "GARM rejection should leave the authentic Mac snapshot prepared without a pool, got {snapshot:?}"
        )
        .into());
    }
    let database = Connection::open(scenario.fixture.directory.join("journal.sqlite3"))?;
    let enabled = database.execute(
        "UPDATE pool_snapshots SET state='enabled', garm_pool_id=?2 WHERE id=?1 AND state='prepared' AND garm_pool_id IS NULL",
        (&snapshot.id, "pool-system-201"),
    )?;
    if enabled != 1 {
        return Err(
            "could not model the GARM-enabled pool after its macOS capability rejection".into(),
        );
    }
    let mut bootstrap: Value =
        serde_json::from_str(include_str!("../fixtures/system/bootstrap.json"))?;
    bootstrap["name"] = json!("runner-macos-system");
    bootstrap["os_type"] = json!("macos");
    bootstrap["arch"] = json!("arm64");
    bootstrap["tools"][0]["os"] = json!("osx");
    bootstrap["tools"][0]["architecture"] = json!("arm64");
    bootstrap["tools"][0]["filename"] = json!("actions-runner-osx-arm64.tar.gz");
    bootstrap["pool_id"] = json!("pool-system-201");
    bootstrap["extra_specs"] = json!({"snapshot_id":snapshot.id});
    let output = provider_process(
        &scenario.config_path,
        "CreateInstance",
        Some("pool-system-201"),
        Some(&serde_json::to_vec(&bootstrap)?),
    )
    .await?;
    if output.status.success() {
        return Err("macOS allocation unexpectedly succeeded".into());
    }
    let response = reqwest::Client::new()
        .post(format!("http://{}/v1/provider", scenario.address))
        .bearer_auth(DAEMON_TOKEN)
        .json(&json!({"command":"CreateInstance","interface_version":"v0.1.0","controller_id":CONTROLLER_ID,"pool_id":"pool-system-201","instance_id":null,"bootstrap":bootstrap,"pool_extra_specs":{"snapshot_id":snapshot.id}}))
        .send()
        .await?;
    let status = response.status();
    let response: Value = response.json().await?;
    if status != StatusCode::SERVICE_UNAVAILABLE
        || response["error"] != "allocation catalog is unavailable"
    {
        return Err(format!(
            "valid signed MacARM64 snapshot did not reach the catalog capability rejection: {response}"
        )
        .into());
    }
    if lifecycle_commands(&scenario.lifecycle_log)
        .is_ok_and(|commands| commands.iter().any(|command| command == "CreateInstance"))
    {
        return Err("macOS failure reached the pinned provider mutation child".into());
    }
    Ok(())
}

pub(super) async fn create_cross_cloud_allocations(
    scenario: &CrossCloudScenario,
) -> Result<(), Box<dyn std::error::Error>> {
    let aws_token = ROUTING_LABEL;
    let aws_snapshot = scenario.send_signed_event(aws_token, 201).await?;
    let aws_create = scenario
        .create_runner(&aws_snapshot, "runner-old-image")
        .await?;
    if !aws_create.status.success() {
        return Err(format!(
            "broad AWS/Azure/GCP allocation failed: {}",
            String::from_utf8_lossy(&aws_create.stderr)
        )
        .into());
    }

    let gcp_token = "outrider:v1:101:201:1:linux-gcp";
    let gcp_snapshot = scenario.send_signed_event(gcp_token, 202).await?;
    let gcp_create = scenario
        .create_runner(&gcp_snapshot, "runner-gcp-system")
        .await?;
    if !gcp_create.status.success() {
        let allocations = scenario
            .journal
            .as_ref()
            .ok_or("cross-cloud journal is closed")?
            .all_allocations()?;
        return Err(format!(
            "GCP constrained allocation failed: {}; durable rows={:?}; GCP requests={:?}",
            String::from_utf8_lossy(&gcp_create.stderr),
            allocations
                .iter()
                .map(|row| (
                    &row.runner_name,
                    &row.state,
                    &row.detail,
                    row.selected_offer_json.is_some()
                ))
                .collect::<Vec<_>>(),
            scenario
                .gcp
                .state
                .requests
                .lock()
                .map_err(|_| "GCP request lock poisoned")?,
        )
        .into());
    }

    let azure_token = "outrider:v1:101:201:1:linux-azure";
    let azure_snapshot = scenario.send_signed_event(azure_token, 203).await?;
    let azure_create = scenario
        .create_runner(&azure_snapshot, "runner-azure-system")
        .await?;
    if !azure_create.status.success() {
        let snapshot = scenario
            .journal
            .as_ref()
            .ok_or("cross-cloud journal is closed")?
            .pool_snapshot(101, azure_token)?
            .ok_or("Azure snapshot missing after failed allocation")?;
        let document: outrider::github::SnapshotDocument =
            serde_json::from_str(&snapshot.config_json)?;
        let azure_allocations = scenario
            .journal
            .as_ref()
            .ok_or("cross-cloud journal is closed")?
            .all_allocations()?;
        return Err(format!(
            "Azure constrained allocation failed: {}; resolved profile={:?}; allocations={azure_allocations:?}; requests={:?}",
            String::from_utf8_lossy(&azure_create.stderr),
            document.resolved_profile,
            scenario.azure.state.requests.lock().map_err(|_| "Azure request lock poisoned")?,
        )
        .into());
    }

    Ok(())
}

pub(super) async fn verify_cross_cloud_allocations(
    scenario: &CrossCloudScenario,
) -> Result<(), Box<dyn std::error::Error>> {
    if scenario.aws.compute_quote_requests.load(Ordering::Relaxed) < 2
        || scenario.azure.state.quote_reads.load(Ordering::Relaxed) < 1
        || scenario.gcp.state.quote_reads.load(Ordering::Relaxed) < 4
        || scenario.azure.state.catalog_reads.load(Ordering::Relaxed) < 3
        || scenario.gcp.state.catalog_reads.load(Ordering::Relaxed) < 4
    {
        return Err(format!(
            "all three concrete cloud catalog and quote HTTP clients must participate: AWS quotes={}, Azure catalog/quote={}/{}, GCP catalog/quote={}/{}",
            scenario.aws.compute_quote_requests.load(Ordering::Relaxed),
            scenario.azure.state.catalog_reads.load(Ordering::Relaxed),
            scenario.azure.state.quote_reads.load(Ordering::Relaxed),
            scenario.gcp.state.catalog_reads.load(Ordering::Relaxed),
            scenario.gcp.state.quote_reads.load(Ordering::Relaxed),
        ).into());
    }

    let allocations = scenario
        .journal
        .as_ref()
        .ok_or("cross-cloud journal is closed")?
        .all_allocations()?;
    if allocations.len() != 3 {
        return Err(
            format!("expected three durable cloud allocations, got {allocations:?}").into(),
        );
    }
    let aws_allocation = allocations
        .iter()
        .find(|allocation| allocation.runner_name == "runner-old-image")
        .ok_or("AWS request allocation missing")?;
    let gcp_allocation = allocations
        .iter()
        .find(|allocation| allocation.runner_name == "runner-gcp-system")
        .ok_or("GCP request allocation missing")?;
    let azure_allocation = allocations
        .iter()
        .find(|allocation| allocation.runner_name == "runner-azure-system")
        .ok_or("Azure request allocation missing")?;
    for (allocation, expected_cloud, expected_binding) in [
        (aws_allocation, "aws", "aws-child"),
        (azure_allocation, "azure", "azure-child"),
        (gcp_allocation, "gcp", "gcp-child"),
    ] {
        let offer: Value = serde_json::from_str(
            allocation
                .selected_offer_json
                .as_deref()
                .ok_or("selected offer missing")?,
        )?;
        let binding: ProviderBinding = serde_json::from_str(
            allocation
                .binding_json
                .as_deref()
                .ok_or("retained provider binding missing")?,
        )?;
        let identity: Value = serde_json::from_str(
            allocation
                .native_identity_json
                .as_deref()
                .ok_or("native identity missing")?,
        )?;
        let expected_native_id = match expected_cloud {
            "aws" => "i-synthetic-001",
            "azure" => {
                "/subscriptions/11111111-1111-4111-8111-111111111111/resourceGroups/runner-azure-system/providers/Microsoft.Compute/virtualMachines/runner-azure-system"
            }
            "gcp" => "1234567890123456789",
            _ => return Err("unexpected selected cloud".into()),
        };
        if offer["candidate"]["cloud"] != expected_cloud
            || offer["candidate"]["price"]["compute"].is_null()
            || offer["candidate"]["price"]["disk"].is_null()
            || offer["candidate"]["price"]["licensing"].is_null()
            || offer["candidate"]["price"]["public_address"].is_null()
            || offer["selection"]["binding_id"] != expected_binding
            || binding.id != expected_binding
            || binding.executable_sha256.is_empty()
            || binding.configuration_sha256.is_empty()
            || identity["controller_id"] != CONTROLLER_ID
            || identity["cloud"] != expected_cloud
            || identity["resource_id"] != expected_native_id
            || (expected_cloud == "azure"
                && identity["dependency_ids"]["vm_id"] != "azure-vm-system-003")
        {
            return Err(format!("selected cloud offer did not retain its quote, pins, and independently observed native identity: {allocation:?}").into());
        }
    }
    let broad_offer: Value = serde_json::from_str(
        aws_allocation
            .selected_offer_json
            .as_deref()
            .ok_or("broad offer missing")?,
    )?;
    if broad_offer["candidate"]["cloud"] == "gcp" {
        return Err(
            "broad request did not allow the GCP-constrained request to select a different child"
                .into(),
        );
    }
    if scenario.aws.native_inspection_mode.load(Ordering::Relaxed) != 1
        || scenario.azure.state.native_reads.load(Ordering::Relaxed) == 0
        || scenario.gcp.state.native_reads.load(Ordering::Relaxed) == 0
    {
        return Err("selected AWS/Azure/GCP native identities were not independently read through their concrete cloud APIs".into());
    }

    for allocation in [aws_allocation, azure_allocation, gcp_allocation] {
        let get = provider_lifecycle_process(
            &scenario.config_path,
            "GetInstance",
            Some(&allocation.pool_id),
            Some(&allocation.allocation_id),
        )
        .await?;
        let list = provider_lifecycle_process(
            &scenario.config_path,
            "ListInstances",
            Some(&allocation.pool_id),
            None,
        )
        .await?;
        if !get.status.success() || !list.status.success() {
            return Err(format!(
                "journal-routed Get/List failed for {}: get={}, list={}",
                allocation.runner_name,
                String::from_utf8_lossy(&get.stderr),
                String::from_utf8_lossy(&list.stderr)
            )
            .into());
        }
        let get_instance: Value = serde_json::from_slice(&get.stdout)?;
        let listed_instances: Vec<Value> = serde_json::from_slice(&list.stdout)?;
        if get_instance["name"] != allocation.runner_name
            || get_instance["provider_id"] != allocation.allocation_id
            || !listed_instances.iter().any(|instance| {
                instance["name"] == allocation.runner_name
                    && instance["provider_id"] == allocation.allocation_id
            })
        {
            return Err(format!(
                "journal-routed Get/List omitted owned {} in pool {}: Get={get_instance:?}; List={listed_instances:?}",
                allocation.allocation_id, allocation.pool_id
            )
            .into());
        }
    }
    drop(allocations);
    Ok(())
}

pub(super) async fn cleanup_cross_cloud_allocations(
    scenario: &mut CrossCloudScenario,
) -> Result<(), Box<dyn std::error::Error>> {
    scenario.restart_services().await?;
    let cleanup = scenario.remove_all().await?;
    if !cleanup.status.success() {
        return Err(format!(
            "cross-cloud RemoveAllInstances failed: {}",
            String::from_utf8_lossy(&cleanup.stderr)
        )
        .into());
    }
    let repeated_cleanup = scenario.remove_all().await?;
    if !repeated_cleanup.status.success() {
        return Err(format!(
            "repeated cross-cloud cleanup failed: {}",
            String::from_utf8_lossy(&repeated_cleanup.stderr)
        )
        .into());
    }
    let final_allocations = scenario
        .journal
        .as_ref()
        .ok_or("restarted cross-cloud journal is closed")?
        .all_allocations()?;
    if final_allocations.len() != 3
        || final_allocations
            .iter()
            .any(|allocation| allocation.state != "deleted")
    {
        return Err(format!("cross-cloud cleanup did not durably delete both owned allocations: {final_allocations:?}").into());
    }
    for cloud in ["aws", "azure", "gcp"] {
        let log = scenario
            .lifecycle_logs
            .get(cloud)
            .ok_or("provider lifecycle log missing")?;
        let commands = lifecycle_commands(log)?;
        for command in [
            "CreateInstance",
            "GetInstance",
            "ListInstances",
            "DeleteInstance",
        ] {
            if !commands.iter().any(|observed| observed == command) {
                return Err(format!(
                    "{cloud} pinned child did not receive journal-routed {command}: {commands:?}"
                )
                .into());
            }
        }
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
            return Err(format!(
                "{cloud} child was created or cleaned more than once: {commands:?}"
            )
            .into());
        }
    }
    if scenario
        .azure
        .state
        .requests
        .lock()
        .map_err(|_| "Azure request lock poisoned")?
        .iter()
        .any(|(method, _)| method != "GET")
        || scenario
            .gcp
            .state
            .requests
            .lock()
            .map_err(|_| "GCP request lock poisoned")?
            .iter()
            .any(|(method, _)| method != "GET")
    {
        return Err("Rust Azure/GCP client issued a mutating cloud API request".into());
    }
    Ok(())
}

#[tokio::test]
async fn signed_event_to_cross_cloud_cleanup() -> Result<(), Box<dyn std::error::Error>> {
    let mut scenario = CrossCloudScenario::start().await?;
    create_cross_cloud_allocations(&scenario).await?;
    verify_cross_cloud_allocations(&scenario).await?;
    cleanup_cross_cloud_allocations(&mut scenario).await
}

#[tokio::test]
async fn tagged_garm_v010_wrapper_process() -> Result<(), Box<dyn std::error::Error>> {
    let scenario = SystemScenario::start().await?;
    scenario.send_signed_event().await?;
    let pool = scenario.pool()?;
    let mut bootstrap: Value =
        serde_json::from_str(include_str!("../fixtures/system/bootstrap.json"))?;
    bootstrap["extra_specs"] = pool["extra_specs"].clone();
    bootstrap["pool_id"] = pool["id"].clone();
    // GARM's Go nil slices marshal as null when optional settings are absent.
    bootstrap["ssh-keys"] = Value::Null;
    bootstrap["user_data_options"]["extra_packages"] = Value::Null;
    let bootstrap_path = scenario.fixture.directory.join("wrapper-bootstrap.json");
    fs::write(&bootstrap_path, serde_json::to_vec(&bootstrap)?)?;
    let wrapper_probe = scenario.fixture.directory.join("garm-wrapper-probe");
    let build = tokio::process::Command::new("cargo")
        .args(["xtask", "build-garm-wrapper-probe"])
        .arg(&wrapper_probe)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .await?;
    if !build.status.success() {
        return Err(format!(
            "tagged GARM v0.2.1 probe build failed: {}",
            String::from_utf8_lossy(&build.stderr)
        )
        .into());
    }
    let report_path = std::env::var_os("OUTRIDER_GARM_WRAPPER_PROBE_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| scenario.fixture.directory.join("garm-wrapper-probe.json"));
    let probe = tokio::process::Command::new(wrapper_probe)
        .args(["-provider", env!("CARGO_BIN_EXE_outrider"), "-config"])
        .arg(&scenario.config_path)
        .arg("-bootstrap")
        .arg(&bootstrap_path)
        .arg("-controller")
        .arg(CONTROLLER_ID)
        .arg("-output")
        .arg(&report_path)
        .output()
        .await?;
    let report: Value = serde_json::from_slice(&fs::read(&report_path)?)?;
    let Some(steps) = report["steps"].as_array() else {
        return Err(format!(
            "GARM wrapper probe omitted steps: {report}; stderr={}",
            String::from_utf8_lossy(&probe.stderr)
        )
        .into());
    };
    let operations = steps
        .iter()
        .map(|step| step["operation"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    let expected = [
        "CreateInstance",
        "GetInstance",
        "ListInstances",
        "StopInstance",
        "StartInstance",
        "DeleteInstance",
        "DeleteInstance repeated",
        "RemoveAllInstances",
    ];
    if !probe.status.success()
        || report["source_commit"] != "154638445c3949c1958b01812f69d9a1e4d82684"
        || report["interface"] != "v0.1.0"
        || operations != expected
        || steps.iter().any(|step| step["passed"] != true)
    {
        return Err(format!(
            "tagged GARM v0.1.0 wrapper probe failed: {}; report={report}",
            String::from_utf8_lossy(&probe.stderr)
        )
        .into());
    }
    Ok(())
}

#[test]
fn required_tools_no_skip() -> Result<(), Box<dyn std::error::Error>> {
    let cloud_init = Command::new("cloud-init").arg("--version").output()?;
    if !cloud_init.status.success() || !String::from_utf8_lossy(&cloud_init.stdout).contains("26.2")
    {
        return Err("cloud-init 26.2 is required for the bootstrap validators".into());
    }
    let powershell = Command::new("pwsh")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "$PSVersionTable.PSVersion.ToString()",
        ])
        .output()?;
    if !powershell.status.success() || String::from_utf8_lossy(&powershell.stdout).trim() != "7.6.7"
    {
        return Err("PowerShell 7.6.7 is required for the bootstrap validators".into());
    }
    let plist =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/system/launch-agent.plist");
    let plutil = Command::new("plutil").arg("-lint").arg(plist).output()?;
    if !plutil.status.success() {
        return Err("native plutil is required for the macOS plist validator".into());
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) enum ExplanationServerResponse {
    OversizedChunked,
    Delayed,
}

pub(super) async fn explanation_response_server(
    listener: TokioTcpListener,
    response: ExplanationServerResponse,
) -> io::Result<()> {
    let (mut stream, _) = listener.accept().await?;
    let mut request = [0_u8; 4096];
    let _ = stream.read(&mut request).await?;
    if matches!(response, ExplanationServerResponse::Delayed) {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let payload = vec![b'a'; 1_000_001];
    let header = format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{:x}\r\n",
        payload.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.write_all(b"\r\n0\r\n\r\n").await?;
    Ok(())
}
