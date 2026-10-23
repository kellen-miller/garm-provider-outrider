use std::{collections::BTreeMap, time::Duration};

use outrider::cloud::{CloudCatalog, OwnershipScope, PlacementIdentity};
use serde_json::Value;

use super::cloud_server::{
    MockResponse, backend, image_response, profile, query, retail_response, sku, start_server,
    vm_response,
};

#[tokio::test]
async fn read_only_cloud_calls() {
    let server = start_server(vec![image_response(), MockResponse { status: 200, body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string() }, retail_response(), vm_response(), MockResponse { status: 200, body: serde_json::json!({"value":[serde_json::from_str::<serde_json::Value>(&vm_response().body).unwrap_or_else(|_| panic!("valid VM fixture"))]}).to_string() }]).await.unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server).await;
    let _ = backend
        .catalog(
            query(profile()),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"))
        .offers;
    let resource_id = "/SUBSCRIPTIONS/11111111-1111-4111-8111-111111111111/RESOURCEGROUPS/GARM/PROVIDERS/MICROSOFT.COMPUTE/VIRTUALMACHINES/RUNNER-1";
    let identity = PlacementIdentity {
        controller_id: "controller-1".to_owned(),
        cloud: "AZURE".to_owned(),
        account_id: "11111111-1111-4111-8111-111111111111".to_ascii_uppercase(),
        location: "EASTUS".to_owned(),
        resource_id: resource_id.to_owned(),
        host_id: None,
        dependency_ids: BTreeMap::new(),
    };
    let inspected = backend
        .inspect(
            &identity,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("inspect"))
        .unwrap_or_else(|| panic!("VM exists"));
    assert_eq!(inspected.state, "running");
    let listed = backend
        .list(
            &OwnershipScope {
                controller_id: "controller-1".to_owned(),
                resource_name: None,
            },
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("list"));
    assert_eq!(listed.len(), 1);
    server
        .task
        .await
        .unwrap_or_else(|_| panic!("server task"))
        .unwrap_or_else(|_| panic!("server"));
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("requests lock"));
    assert_eq!(requests.len(), 5);
    assert!(requests.iter().all(|request| request.method == "GET"));
    assert!(
        requests
            .iter()
            .filter(|request| request.path.contains("Microsoft.Compute"))
            .all(|request| request.authorization.as_deref() == Some("Bearer local-test-token"))
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.path.starts_with("/api/retail/prices"))
            .all(|request| request.authorization.is_none())
    );
}

#[tokio::test]
async fn resource_health_interruption_observation_uses_exact_timed_annotation() {
    let source: Value = serde_json::from_str(include_str!(
        "../acceptance/fixtures/azure-interruption-identity.json"
    ))
    .unwrap_or_else(|_| panic!("Azure interruption source fixture"));
    let response = source["response"].to_string();
    let server = start_server(vec![
        MockResponse {
            status: 404,
            body: "{}".to_owned(),
        },
        MockResponse {
            status: 200,
            body: response,
        },
    ])
    .await
    .unwrap_or_else(|_| panic!("Resource Graph fixture server"));
    let backend = backend(&server).await;
    let identity = PlacementIdentity {
        controller_id: "controller-1".to_owned(),
        cloud: "azure".to_owned(),
        account_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        location: "eastus".to_owned(),
        resource_id: source["allocation"]["arm_path"]
            .as_str()
            .unwrap_or_else(|| panic!("ARM identity"))
            .to_owned(),
        host_id: None,
        dependency_ids: BTreeMap::from([(
            "vm_id".to_owned(),
            source["allocation"]["vm_id"]
                .as_str()
                .unwrap_or_else(|| panic!("VM generation"))
                .to_owned(),
        )]),
    };
    let evidence = backend
        .observe_interruption(
            &identity,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("Resource Graph query"));
    let evidence = evidence
        .first()
        .unwrap_or_else(|| panic!("verified Resource Health annotation"));
    assert!(evidence.source_id.ends_with("/synthetic-event-1"));
    assert!(
        evidence
            .event_id
            .ends_with("/synthetic-event-1@2026-10-21T16:00:00Z")
    );
    assert_eq!(evidence.occurred_at, "2026-10-21T16:00:00Z");
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request capture"));
    let request = requests
        .first()
        .unwrap_or_else(|| panic!("Resource Graph request"));
    assert_eq!(request.method, "GET");
    assert_eq!(
        request.authorization.as_deref(),
        Some("Bearer local-test-token")
    );
    assert!(
        request
            .path
            .contains("/virtualMachines/runner-1?api-version=2025-04-01")
    );
    let resource_graph_request = requests
        .get(1)
        .unwrap_or_else(|| panic!("Resource Graph request"));
    assert_eq!(resource_graph_request.method, "POST");
    assert!(
        resource_graph_request
            .path
            .contains("api-version=2024-04-01")
    );
}

#[tokio::test]
async fn resource_graph_identity_and_partial_results_stay_unknown() {
    let mut source: Value = serde_json::from_str(include_str!(
        "../acceptance/fixtures/azure-interruption-identity.json"
    ))
    .unwrap_or_else(|_| panic!("Azure interruption source fixture"));
    source["response"]["data"][0]["properties"]["targetResourceId"] = serde_json::json!(
        "/subscriptions/11111111-1111-4111-8111-111111111111/resourceGroups/foreign/providers/Microsoft.Compute/virtualMachines/runner-1"
    );
    let server = start_server(vec![
        MockResponse {
            status: 404,
            body: "{}".to_owned(),
        },
        MockResponse {
            status: 200,
            body: source["response"].to_string(),
        },
    ])
    .await
    .unwrap_or_else(|_| panic!("foreign annotation fixture server"));
    let foreign_backend = backend(&server).await;
    let identity = PlacementIdentity {
        controller_id: "controller-1".to_owned(),
        cloud: "azure".to_owned(),
        account_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        location: "eastus".to_owned(),
        resource_id: source["allocation"]["arm_path"]
            .as_str()
            .unwrap_or_else(|| panic!("ARM identity"))
            .to_owned(),
        host_id: None,
        dependency_ids: BTreeMap::from([(
            "vm_id".to_owned(),
            "22222222-2222-4222-8222-222222222222".to_owned(),
        )]),
    };
    assert!(
        foreign_backend
            .observe_interruption(
                &identity,
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .unwrap_or_else(|_| panic!("foreign annotation lookup"))
            .is_empty()
    );

    source["response"]["data"] = serde_json::json!([]);
    source["response"]["resultTruncated"] = serde_json::json!("true");
    let server = start_server(vec![
        MockResponse {
            status: 404,
            body: "{}".to_owned(),
        },
        MockResponse {
            status: 200,
            body: source["response"].to_string(),
        },
    ])
    .await
    .unwrap_or_else(|_| panic!("truncated annotation fixture server"));
    let backend = backend(&server).await;
    let error = backend
        .observe_interruption(
            &identity,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect_err("truncated result without continuation is unknown");
    assert!(error.to_string().contains("truncated"));
}

#[tokio::test]
async fn resource_graph_positive_first_page_does_not_hide_failed_continuation() {
    let mut source: Value = serde_json::from_str(include_str!(
        "../acceptance/fixtures/azure-interruption-identity.json"
    ))
    .unwrap_or_else(|_| panic!("Azure interruption source fixture"));
    source["response"]["$skipToken"] = serde_json::json!("next-page");
    let server = start_server(vec![
        MockResponse {
            status: 404,
            body: "{}".to_owned(),
        },
        MockResponse {
            status: 200,
            body: source["response"].to_string(),
        },
        MockResponse {
            status: 403,
            body: "{\"error\":\"page unavailable\"}".to_owned(),
        },
    ])
    .await
    .unwrap_or_else(|_| panic!("Resource Graph continuation fixture server"));
    let backend = backend(&server).await;
    let identity = PlacementIdentity {
        controller_id: "controller-1".to_owned(),
        cloud: "azure".to_owned(),
        account_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        location: "eastus".to_owned(),
        resource_id: source["allocation"]["arm_path"]
            .as_str()
            .unwrap_or_else(|| panic!("ARM identity"))
            .to_owned(),
        host_id: None,
        dependency_ids: BTreeMap::from([(
            "vm_id".to_owned(),
            source["allocation"]["vm_id"]
                .as_str()
                .unwrap_or_else(|| panic!("VM generation"))
                .to_owned(),
        )]),
    };
    let error = backend
        .observe_interruption(
            &identity,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect_err("positive first page cannot hide a failed continuation");
    assert!(
        error
            .to_string()
            .contains("Resource Graph request returned an error"),
        "unexpected continuation error: {error}"
    );
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request capture"));
    assert_eq!(requests.len(), 3);
    assert!(requests[2].path.contains("api-version=2024-04-01"));
}

#[tokio::test]
async fn resource_graph_conflicting_same_revision_is_unknown() {
    let mut source: Value = serde_json::from_str(include_str!(
        "../acceptance/fixtures/azure-interruption-identity.json"
    ))
    .unwrap_or_else(|_| panic!("Azure interruption source fixture"));
    let mut conflicting = source["response"]["data"][0].clone();
    conflicting["properties"]["context"] = serde_json::json!("User-Initiated");
    source["response"]["data"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("Resource Graph annotation rows"))
        .push(conflicting);
    let server = start_server(vec![
        MockResponse {
            status: 404,
            body: "{}".to_owned(),
        },
        MockResponse {
            status: 200,
            body: source["response"].to_string(),
        },
    ])
    .await
    .unwrap_or_else(|_| panic!("conflicting Resource Graph fixture server"));
    let backend = backend(&server).await;
    let identity = PlacementIdentity {
        controller_id: "controller-1".to_owned(),
        cloud: "azure".to_owned(),
        account_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        location: "eastus".to_owned(),
        resource_id: source["allocation"]["arm_path"]
            .as_str()
            .unwrap_or_else(|| panic!("ARM identity"))
            .to_owned(),
        host_id: None,
        dependency_ids: BTreeMap::from([(
            "vm_id".to_owned(),
            source["allocation"]["vm_id"]
                .as_str()
                .unwrap_or_else(|| panic!("VM generation"))
                .to_owned(),
        )]),
    };
    let error = backend
        .observe_interruption(
            &identity,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect_err("conflicting same-revision annotation facts are unknown");
    assert!(error.to_string().contains("conflicting facts"));
}

#[tokio::test]
async fn resource_graph_rejects_replaced_current_vm_incarnation() {
    let source: Value = serde_json::from_str(include_str!(
        "../acceptance/fixtures/azure-interruption-identity.json"
    ))
    .unwrap_or_else(|_| panic!("Azure interruption source fixture"));
    let server = start_server(vec![
        vm_response(),
        MockResponse {
            status: 200,
            body: source["response"].to_string(),
        },
    ])
    .await
    .unwrap_or_else(|_| panic!("replacement incarnation fixture server"));
    let backend = backend(&server).await;
    let identity = PlacementIdentity {
        controller_id: "controller-1".to_owned(),
        cloud: "azure".to_owned(),
        account_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        location: "eastus".to_owned(),
        resource_id: "/subscriptions/11111111-1111-4111-8111-111111111111/resourceGroups/garm/providers/Microsoft.Compute/virtualMachines/runner-1".to_owned(),
        host_id: None,
        dependency_ids: BTreeMap::from([(
            "vm_id".to_owned(),
            "replacement-incarnation".to_owned(),
        )]),
    };
    let error = backend
        .observe_interruption(
            &identity,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect_err("a replacement VM cannot inherit old annotation evidence");
    assert!(error.to_string().contains("identity"));
}
