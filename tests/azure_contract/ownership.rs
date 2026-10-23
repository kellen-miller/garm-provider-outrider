use std::{collections::BTreeMap, sync::Arc, time::Duration};

use outrider::cloud::{CloudCatalog, OwnershipScope, PlacementIdentity, azure::AzureBackend};

use super::cloud_server::{
    MockResponse, TestCredential, backend, child_config, image_response, profile, query,
    retail_response, settings, sku, start_server, vm_response,
};

#[tokio::test]
async fn provider_ownership_observations() {
    let owned_vm: serde_json::Value =
        serde_json::from_str(&vm_response().body).unwrap_or_else(|_| panic!("VM fixture"));
    let mut foreign_vm = owned_vm.clone();
    foreign_vm["id"] = serde_json::json!(
        "/subscriptions/11111111-1111-4111-8111-111111111111/resourceGroups/garm/providers/Microsoft.Compute/virtualMachines/runner-foreign"
    );
    foreign_vm["name"] = serde_json::json!("runner-foreign");
    foreign_vm["tags"]["garm-controller-id"] = serde_json::json!("other-controller");
    let mut other_owned_vm = owned_vm.clone();
    other_owned_vm["id"] = serde_json::json!(
        "/subscriptions/11111111-1111-4111-8111-111111111111/resourceGroups/garm/providers/Microsoft.Compute/virtualMachines/runner-2"
    );
    other_owned_vm["name"] = serde_json::json!("runner-2");
    let mut missing_incarnation_vm = owned_vm.clone();
    if let Some(properties) = missing_incarnation_vm["properties"].as_object_mut() {
        properties.remove("vmId");
        properties.remove("instanceView");
    }
    let mut stopped_vm = owned_vm.clone();
    stopped_vm["properties"]["instanceView"]["statuses"][0]["code"] =
        serde_json::json!("PowerState/stopped");
    let mut replacement_vm = owned_vm.clone();
    replacement_vm["properties"]["vmId"] =
        serde_json::json!("00000000-0000-4000-8000-000000000099");
    let server = start_server(vec![
        vm_response(),
        MockResponse {
            status: 200,
            body: replacement_vm.to_string(),
        },
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[owned_vm, other_owned_vm, foreign_vm]}).to_string(),
        },
        MockResponse {
            status: 503,
            body: r#"{"error":"synthetic unavailable"}"#.to_owned(),
        },
        MockResponse {
            status: 200,
            body: missing_incarnation_vm.to_string(),
        },
        MockResponse {
            status: 200,
            body: stopped_vm.to_string(),
        },
    ])
    .await
    .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server).await;
    let resource_id = "/subscriptions/11111111-1111-4111-8111-111111111111/resourceGroups/garm/providers/Microsoft.Compute/virtualMachines/runner-1";
    let identity = PlacementIdentity {
        controller_id: "controller-1".to_owned(),
        cloud: "azure".to_owned(),
        account_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        location: "eastus".to_owned(),
        resource_id: resource_id.to_owned(),
        host_id: None,
        dependency_ids: BTreeMap::from([(
            "vm_id".to_owned(),
            "00000000-0000-4000-8000-000000000001".to_owned(),
        )]),
    };

    let inspected = backend
        .inspect(
            &identity,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("inspection succeeds"))
        .unwrap_or_else(|| panic!("owned VM exists"));
    assert_eq!(inspected.identity.location, "eastus");
    assert_eq!(
        inspected.identity.account_id,
        "11111111-1111-4111-8111-111111111111"
    );
    assert_eq!(
        inspected.identity.resource_id.to_ascii_lowercase(),
        resource_id.to_ascii_lowercase()
    );
    assert_eq!(
        inspected.identity.dependency_ids.get("vm_id"),
        Some(&"00000000-0000-4000-8000-000000000001".to_owned())
    );
    assert!(
        inspected
            .ownership_tags
            .contains(&("garm-pool-id".to_owned(), "pool-1".to_owned()))
    );
    assert!(
        backend
            .inspect(
                &identity,
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err(),
        "a reused ARM path with a different vmId must not match the stored incarnation"
    );
    let listed = backend
        .list(
            &OwnershipScope {
                controller_id: "controller-1".to_owned(),
                resource_name: Some("runner-1".to_owned()),
            },
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("inventory succeeds"));
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].identity.resource_id, resource_id);
    assert!(
        backend
            .inspect(
                &identity,
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err(),
        "an ARM outage must not become an absent VM"
    );
    assert!(
        backend
            .inspect(
                &identity,
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .is_err(),
        "missing vmId cannot satisfy a retained incarnation"
    );
    let stopped = backend
        .inspect(
            &identity,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("stopped VM can be observed"))
        .unwrap_or_else(|| panic!("stopped VM exists"));
    assert_eq!(stopped.state, "stopped");
    server
        .task
        .await
        .unwrap_or_else(|_| panic!("server task"))
        .unwrap_or_else(|_| panic!("server"));
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("requests lock"))
            .len(),
        6
    );
}

#[tokio::test]
async fn owned_inventory_rejects_malformed_native_scope() {
    let mut foreign_subscription: serde_json::Value =
        serde_json::from_str(&vm_response().body).unwrap_or_else(|_| panic!("VM fixture"));
    foreign_subscription["id"] = serde_json::json!(
        "/subscriptions/other/resourceGroups/garm/providers/Microsoft.Compute/virtualMachines/runner-1"
    );
    let mut empty_name: serde_json::Value =
        serde_json::from_str(&vm_response().body).unwrap_or_else(|_| panic!("VM fixture"));
    empty_name["name"] = serde_json::json!("");
    for malformed in [foreign_subscription, empty_name] {
        let server = start_server(vec![MockResponse {
            status: 200,
            body: serde_json::json!({"value":[malformed]}).to_string(),
        }])
        .await
        .unwrap_or_else(|_| panic!("mock server"));
        let backend = backend(&server).await;
        let result = backend
            .list(
                &OwnershipScope {
                    controller_id: "controller-1".to_owned(),
                    resource_name: None,
                },
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await;
        assert!(
            result.is_err(),
            "owned malformed native scope cannot appear absent"
        );
        server
            .task
            .await
            .unwrap_or_else(|_| panic!("server task"))
            .unwrap_or_else(|_| panic!("server"));
    }
}

#[tokio::test]
async fn interruption_requires_incarnation() {
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
            query(profile()),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"))
        .offers;
    assert!(
        !offers.is_empty(),
        "interruption assertions require catalog offers"
    );
    assert!(
        offers
            .iter()
            .all(|offer| offer.candidate.interruption_rate_per_hour.is_none()
                && offer.candidate.interruption_samples == 0)
    );
    assert!(
        offers
            .iter()
            .all(|offer| offer.candidate.boot_disk_gib >= 30)
    );
}

#[tokio::test]
async fn binding_subscription_location_mismatch() {
    let bytes = child_config();
    let path = std::env::temp_dir().join(format!(
        "outrider-azure-mismatch-{}.toml",
        std::process::id()
    ));
    std::fs::write(&path, bytes).unwrap_or_else(|_| panic!("write config"));
    let mut settings = settings("https://management.azure.com", path);
    settings.subscription_id = "different-subscription".to_owned();
    let client = reqwest::Client::new();
    let result = AzureBackend::with_clients(
        settings.clone(),
        client,
        Arc::new(TestCredential),
        tokio::time::Instant::now() + Duration::from_secs(2),
    )
    .await;
    assert!(result.is_err());

    settings.subscription_id = "11111111-1111-4111-8111-111111111111".to_owned();
    settings.pricing_location = "westus".to_owned();
    let result = AzureBackend::with_clients(
        settings,
        reqwest::Client::new(),
        Arc::new(TestCredential),
        tokio::time::Instant::now() + Duration::from_secs(2),
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn untrusted_pagination_rejected() {
    let malicious = MockResponse {
        status: 200,
        body: serde_json::json!({"value":[],"nextLink":"https://attacker.invalid/steal"})
            .to_string(),
    };
    let server = start_server(vec![image_response(), malicious])
        .await
        .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server).await;
    let result = backend
        .catalog(
            query(profile()),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await;
    assert!(result.is_err());
    server
        .task
        .await
        .unwrap_or_else(|_| panic!("server task"))
        .unwrap_or_else(|_| panic!("server"));
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("requests lock"))
            .len(),
        2
    );
}
