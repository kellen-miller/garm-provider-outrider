use std::time::Duration;

use outrider::cloud::inventory::{CloudInventory, InventoryKind, InventoryOwnership};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::cloud_server::{MockResponse, backend, start_server};

const SUBSCRIPTION: &str = "11111111-1111-4111-8111-111111111111";

fn native_resource(kind: InventoryKind) -> Value {
    let resource_type = match kind {
        InventoryKind::VirtualMachine => "Microsoft.Compute/virtualMachines",
        InventoryKind::Disk => "Microsoft.Compute/disks",
        InventoryKind::AllocatedAddress => "Microsoft.Network/publicIPAddresses",
    };
    json!({
        "id":format!("/subscriptions/{SUBSCRIPTION}/resourceGroups/runner-rg/providers/{resource_type}/native-resource"),
        "name":"native-resource", "location":"eastus",
        "tags":{"GARM-CONTROLLER-ID":"controller-1", "secret":"never-export-this"},
        "properties":{"vmId":"incarnation-1", "uniqueId":"disk-1", "provisioningState":"Succeeded", "osProfile":{"customData":"never-export-this"}}
    })
}

#[tokio::test]
async fn subscription_inventory_keeps_all_owners_and_omits_sensitive_fields() {
    for kind in InventoryKind::ALL {
        let row = native_resource(kind);
        let mut other = row.clone();
        other["id"] = json!(format!("{}-other", row["id"].as_str().unwrap()));
        other["name"] = json!("native-resource-other");
        other["tags"]["GARM-CONTROLLER-ID"] = json!("never-export-this");
        let mut untagged = row.clone();
        untagged["id"] = json!(format!("{}-untagged", row["id"].as_str().unwrap()));
        untagged["name"] = json!("native-resource-untagged");
        untagged["tags"] = Value::Null;
        let server = start_server(vec![MockResponse {
            status: 200,
            body: json!({"value":[row, other, untagged]}).to_string(),
        }])
        .await
        .unwrap();
        let backend = backend(&server).await;
        let collection = backend
            .inventory(
                kind,
                "controller-1",
                Instant::now() + Duration::from_secs(3),
            )
            .await
            .unwrap();
        assert_eq!(collection.resources.len(), 3);
        assert_eq!(
            collection.resources[0].ownership,
            InventoryOwnership::Controller
        );
        assert_eq!(
            collection.resources[1].ownership,
            InventoryOwnership::OtherController
        );
        assert_eq!(
            collection.resources[2].ownership,
            InventoryOwnership::Untagged
        );
        if kind == InventoryKind::VirtualMachine {
            assert_eq!(collection.resources[0].state, None);
        }
        assert!(
            !serde_json::to_string(&collection)
                .unwrap()
                .contains("never-export-this")
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests[0].method, "GET");
        assert!(!requests[0].path.contains("statusOnly"));
        assert!(!requests[0].path.contains("filter"));
        server.task.abort();
    }
}

#[tokio::test]
async fn oversized_native_page_is_rejected_before_json_decoding() {
    let body = format!("{}{{\"value\":[]}}", " ".repeat(32 * 1024 * 1024));
    let server = start_server(vec![MockResponse { status: 200, body }])
        .await
        .unwrap();
    let error = backend(&server)
        .await
        .inventory(
            InventoryKind::Disk,
            "controller-1",
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error,outrider::cloud::CloudError::Failure(failure) if failure.kind == "response")
    );
    server.task.abort();
}

#[tokio::test]
async fn malformed_or_foreign_native_resources_are_not_empty_success() {
    let mut foreign = native_resource(InventoryKind::Disk);
    foreign["id"] = json!(
        foreign["id"]
            .as_str()
            .unwrap()
            .replace(SUBSCRIPTION, "foreign-subscription")
    );
    let mut missing_incarnation = native_resource(InventoryKind::Disk);
    missing_incarnation["properties"]["uniqueId"] = Value::Null;
    let mut duplicate_owner = native_resource(InventoryKind::Disk);
    duplicate_owner["tags"]["garm-controller-id"] = json!("different-controller");
    for body in [
        json!({}),
        json!({"value":[foreign]}),
        json!({"value":[missing_incarnation]}),
        json!({"value":[duplicate_owner]}),
    ] {
        let server = start_server(vec![MockResponse {
            status: 200,
            body: body.to_string(),
        }])
        .await
        .unwrap();
        assert!(
            backend(&server)
                .await
                .inventory(
                    InventoryKind::Disk,
                    "controller-1",
                    Instant::now() + Duration::from_secs(3)
                )
                .await
                .is_err()
        );
        server.task.abort();
    }
}

#[tokio::test]
async fn incomplete_inventory_and_untrusted_continuations_are_rejected() {
    for status in [200, 403] {
        let server = start_server(vec![
            MockResponse {status:200,body:json!({"value":[],"nextLink":"{{endpoint}}/subscriptions/11111111-1111-4111-8111-111111111111/providers/Microsoft.Compute/disks?api-version=2025-01-02&page=2"}).to_string()},
            MockResponse {status,body:json!({"value":[],"nextLink":"https://foreign.invalid/steal"}).to_string()},
        ]).await.unwrap();
        assert!(
            backend(&server)
                .await
                .inventory(
                    InventoryKind::Disk,
                    "controller-1",
                    Instant::now() + Duration::from_secs(3)
                )
                .await
                .is_err()
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        server.task.abort();
    }
}
