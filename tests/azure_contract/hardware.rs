use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use outrider::{
    cloud::{CloudCatalog, azure::AzureBackend},
    config::AzureAcceleratorFacts,
};
use rust_decimal::Decimal;

use super::cloud_server::{
    MockResponse, TestCredential, backend, child_config, image_response, profile, query,
    retail_response, settings, sku, start_server,
};

#[tokio::test]
async fn sku_architecture_family_and_constrained_cpu() {
    let mut wrong_architecture = sku("Standard_D2s_v3");
    wrong_architecture["capabilities"][0]["value"] = serde_json::json!("Arm64");
    let mut wrong_family = sku("Standard_D2s_v3");
    wrong_family["family"] = serde_json::json!("OtherFamily");
    let mut profile = profile();
    profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    profile.machine_families = BTreeMap::from([("azure".to_owned(), vec!["Dsv3".to_owned()])]);
    profile
        .administrator_limits
        .allowed_machine_families
        .insert("azure".to_owned(), BTreeSet::from(["Dsv3".to_owned()]));
    let server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku("Standard_D2s_v3"), wrong_architecture, wrong_family]}).to_string(),
        },
        retail_response(),
    ])
    .await
    .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server).await;
    let discovery = backend
        .catalog(
            query(profile),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"));
    let offers = discovery.offers;
    assert!(discovery.queried);
    assert!(discovery.exclusions.iter().any(|exclusion| {
        exclusion.reason == "SKU CPU architecture does not match the image"
            && exclusion.observed_count == 1
    }));
    assert!(discovery.exclusions.iter().any(|exclusion| {
        exclusion.reason == "SKU machine family is outside the allowed families"
            && exclusion.observed_count == 1
    }));
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].candidate.machine_family.as_deref(), Some("Dsv3"));
    assert_eq!(offers[0].candidate.vcpu, 8);
    assert_eq!(
        offers[0].candidate.boot_disk_throughput_mib_s,
        Decimal::new(57_220_458_984_375, 12)
    );
}

#[tokio::test]
async fn gpu_uses_central_exact_sku_facts() {
    let mut gpu_sku = sku("Standard_NC4as_T4_v3");
    gpu_sku["family"] = serde_json::json!("standardNCasT4v3Family");
    gpu_sku["capabilities"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("capability array"))
        .extend([
            serde_json::json!({"name":"GPUsAvailable","value":"1"}),
            serde_json::json!({"name":"GpuMemoryGB","value":"15.5"}),
        ]);
    let mut retail = retail_response();
    let mut retail_body: serde_json::Value =
        serde_json::from_str(&retail.body).unwrap_or_else(|_| panic!("retail fixture"));
    if let Some(items) = retail_body["Items"].as_array_mut() {
        for item in items
            .iter_mut()
            .filter(|item| item["serviceName"] == "Virtual Machines")
        {
            item["armSkuName"] = serde_json::json!("Standard_NC4as_T4_v3");
        }
    }
    retail.body = retail_body.to_string();
    let path = std::env::temp_dir().join(format!("outrider-azure-gpu-{}.toml", std::process::id()));
    std::fs::write(&path, child_config()).unwrap_or_else(|_| panic!("write config"));
    let server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[gpu_sku]}).to_string(),
        },
        retail,
    ])
    .await
    .unwrap_or_else(|_| panic!("mock server"));
    let mut azure_settings = settings(&server.endpoint, path);
    azure_settings.accelerator_facts.insert(
        "Standard_NC4as_T4_v3".to_owned(),
        AzureAcceleratorFacts {
            vendor: "NVIDIA".to_owned(),
            model: "T4".to_owned(),
            total_memory_gib: Decimal::new(155, 1),
            source: "operator verified NCasT4v3 hardware documentation".to_owned(),
        },
    );
    let backend = AzureBackend::with_clients(
        azure_settings,
        reqwest::Client::new(),
        Arc::new(TestCredential),
        tokio::time::Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap_or_else(|_| panic!("valid Azure binding"));
    let mut requested = profile();
    requested
        .machine_types
        .insert("azure".to_owned(), vec!["Standard_NC4as_T4_v3".to_owned()]);
    requested.administrator_limits.allowed_machine_types.insert(
        "azure".to_owned(),
        BTreeSet::from(["Standard_NC4as_T4_v3".to_owned()]),
    );
    requested.machine_families.insert(
        "azure".to_owned(),
        vec!["standardNCasT4v3Family".to_owned()],
    );
    requested.gpu = Some(outrider::config::GpuRequirement {
        vendor: "nvidia".to_owned(),
        model: Some("T4".to_owned()),
        count: 1,
        memory_gib: Some(15),
    });
    requested.administrator_limits.max_gpu_count = 1;
    requested.administrator_limits.max_gpu_memory_gib = 16;
    requested.administrator_limits.allowed_gpu_vendors = BTreeSet::from(["nvidia".to_owned()]);
    requested.administrator_limits.allowed_gpu_models = BTreeSet::from(["T4".to_owned()]);
    requested.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    let offers = backend
        .catalog(
            query(requested),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("GPU catalog"))
        .offers;
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].candidate.gpu_count, 1);
    assert_eq!(
        offers[0].candidate.gpu_memory_gib,
        Some(Decimal::new(155, 1))
    );
    server
        .task
        .await
        .unwrap_or_else(|_| panic!("server task"))
        .unwrap_or_else(|_| panic!("server"));
}

#[tokio::test]
async fn local_scratch_requires_reported_capacity() {
    let mut reported_scratch = sku("Standard_D2s_v3");
    reported_scratch["capabilities"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("capability array"))
        .push(serde_json::json!({"name":"MaxResourceVolumeMB","value":"10240"}));
    let mut request = profile();
    request.local_scratch_gib = 10;
    request.administrator_limits.max_local_scratch_gib = 10;
    request.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    let server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[reported_scratch]}).to_string(),
        },
        retail_response(),
    ])
    .await
    .unwrap_or_else(|_| panic!("mock server"));
    let scratch_backend = backend(&server).await;
    let offers = scratch_backend
        .catalog(
            query(request.clone()),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"))
        .offers;
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].candidate.local_scratch_gib, Decimal::from(10));
    server
        .task
        .await
        .unwrap_or_else(|_| panic!("server task"))
        .unwrap_or_else(|_| panic!("server"));

    let server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
        },
        retail_response(),
    ])
    .await
    .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server).await;
    let offers = backend
        .catalog(
            query(request),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"))
        .offers;
    assert!(
        offers.is_empty(),
        "unknown scratch capacity cannot meet a positive request"
    );
    server
        .task
        .await
        .unwrap_or_else(|_| panic!("server task"))
        .unwrap_or_else(|_| panic!("server"));
}

#[tokio::test]
async fn gpu_unknown_and_mismatched_facts_are_rejected() {
    let mut gpu_sku = sku("Standard_NC4as_T4_v3");
    gpu_sku["family"] = serde_json::json!("standardNCasT4v3Family");
    gpu_sku["capabilities"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("capability array"))
        .extend([
            serde_json::json!({"name":"GPUsAvailable","value":"1"}),
            serde_json::json!({"name":"GpuMemoryGB","value":"15.5"}),
        ]);
    let mismatched = AzureAcceleratorFacts {
        vendor: "NVIDIA".to_owned(),
        model: "T4".to_owned(),
        total_memory_gib: Decimal::from(24),
        source: "operator verified synthetic T4 hardware row with conflicting memory".to_owned(),
    };
    for (index, facts) in [None, Some(mismatched)].into_iter().enumerate() {
        let mut retail = retail_response();
        let mut retail_body: serde_json::Value =
            serde_json::from_str(&retail.body).unwrap_or_else(|_| panic!("retail fixture"));
        if let Some(items) = retail_body["Items"].as_array_mut() {
            for item in items
                .iter_mut()
                .filter(|item| item["serviceName"] == "Virtual Machines")
            {
                item["armSkuName"] = serde_json::json!("Standard_NC4as_T4_v3");
            }
        }
        retail.body = retail_body.to_string();
        let path = std::env::temp_dir().join(format!(
            "outrider-azure-gpu-unknown-{}-{index}.toml",
            std::process::id()
        ));
        std::fs::write(&path, child_config()).unwrap_or_else(|_| panic!("write config"));
        let server = start_server(vec![
            image_response(),
            MockResponse {
                status: 200,
                body: serde_json::json!({"value":[gpu_sku.clone()]}).to_string(),
            },
            retail,
        ])
        .await
        .unwrap_or_else(|_| panic!("mock server"));
        let mut azure_settings = settings(&server.endpoint, path);
        if let Some(facts) = facts {
            azure_settings
                .accelerator_facts
                .insert("Standard_NC4as_T4_v3".to_owned(), facts);
        }
        let backend = AzureBackend::with_clients(
            azure_settings,
            reqwest::Client::new(),
            Arc::new(TestCredential),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("valid Azure binding"));
        let mut requested = profile();
        requested
            .machine_types
            .insert("azure".to_owned(), vec!["Standard_NC4as_T4_v3".to_owned()]);
        requested.administrator_limits.allowed_machine_types.insert(
            "azure".to_owned(),
            BTreeSet::from(["Standard_NC4as_T4_v3".to_owned()]),
        );
        requested.machine_families.insert(
            "azure".to_owned(),
            vec!["standardNCasT4v3Family".to_owned()],
        );
        requested.gpu = Some(outrider::config::GpuRequirement {
            vendor: "nvidia".to_owned(),
            model: Some("T4".to_owned()),
            count: 1,
            memory_gib: Some(15),
        });
        requested.administrator_limits.max_gpu_count = 1;
        requested.administrator_limits.max_gpu_memory_gib = 32;
        requested.administrator_limits.allowed_gpu_vendors = BTreeSet::from(["nvidia".to_owned()]);
        requested.administrator_limits.allowed_gpu_models = BTreeSet::from(["T4".to_owned()]);
        requested.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
        let offers = backend
            .catalog(
                query(requested),
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap_or_else(|_| panic!("GPU catalog"))
            .offers;
        assert!(
            offers.is_empty(),
            "GPU candidates require matching trusted exact-SKU facts"
        );
        server
            .task
            .await
            .unwrap_or_else(|_| panic!("server task"))
            .unwrap_or_else(|_| panic!("server"));
    }
}

#[tokio::test]
async fn public_ip_and_disk_dimensions() {
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("Standard_LRS", 32),
        Some("S4")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("Standard_LRS", 40),
        Some("S6")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("Standard_LRS", 127),
        Some("S10")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("Standard_LRS", 129),
        Some("S15")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("Standard_LRS", 4096),
        Some("S50")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("Standard_LRS", 4097),
        Some("S60")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("Premium_LRS", 4),
        Some("P1")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("Premium_LRS", 8),
        Some("P2")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("Premium_LRS", 16),
        Some("P3")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("Premium_LRS", 32),
        Some("P4")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("Premium_LRS", 64),
        Some("P6")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("Premium_LRS", 127),
        Some("P10")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("StandardSSD_LRS", 4),
        Some("E1")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("StandardSSD_LRS", 40),
        Some("E6")
    );
    assert_eq!(
        outrider::cloud::azure::disk_meter_tier("StandardSSD_LRS", 127),
        Some("E10")
    );
    let path = std::env::temp_dir().join(format!(
        "outrider-azure-public-ip-{}.toml",
        std::process::id()
    ));
    std::fs::write(&path, child_config()).unwrap_or_else(|_| panic!("write child config"));
    let mut settings = settings("https://management.azure.com", path);
    settings
        .locations
        .get_mut("eastus")
        .unwrap_or_else(|| panic!("location"))
        .public_ip_sku = Some("Standard".to_owned());
    let client = reqwest::Client::new();
    let result = AzureBackend::with_clients(
        settings,
        client,
        Arc::new(TestCredential),
        tokio::time::Instant::now() + Duration::from_secs(2),
    )
    .await;
    assert!(
        result.is_err(),
        "qualified child does not dispatch a selected public IP SKU"
    );
}

#[tokio::test]
async fn os_disk_limit_is_separate_from_billing_tier() {
    let mut profile = profile();
    profile.boot_disk.size_gib = 4096;
    profile.administrator_limits.max_boot_disk_gib = 5000;
    profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    let server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
        },
        retail_response(),
    ])
    .await
    .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server).await;
    let offers = backend
        .catalog(
            query(profile),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"))
        .offers;
    assert!(
        offers.is_empty(),
        "4096GiB belongs to S50, above Azure's 4095GiB OS disk maximum"
    );
    server
        .task
        .await
        .unwrap_or_else(|_| panic!("server task"))
        .unwrap_or_else(|_| panic!("server"));
}
