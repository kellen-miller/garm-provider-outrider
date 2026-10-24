use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use http_body_util::Full;
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use outrider::cloud::inventory::{CloudInventory, InventoryKind, InventoryOwnership};
use serde_json::{Value, json};
use tokio::{net::TcpListener, time::Instant};

use super::{
    cloud_server::{MockServer, RecordedRequest},
    identity::backend,
};

async fn inventory_server(responses: Vec<(u16, Value)>) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|_| panic!("inventory listener"));
    let endpoint = format!(
        "http://{}",
        listener.local_addr().unwrap_or_else(|_| panic!("address"))
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let task = tokio::spawn(async move {
        for (status, body) in responses {
            let (stream, _) = listener.accept().await?;
            let recorded = recorded.clone();
            let response_body = body.to_string();
            tokio::spawn(async move {
                let service = service_fn(move |request: hyper::Request<Incoming>| {
                    let recorded = recorded.clone();
                    let response_body = response_body.clone();
                    inventory_response(request, recorded, status, response_body)
                });
                let _result = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }

        Ok(())
    });
    MockServer {
        endpoint,
        requests,
        task,
    }
}

fn inventory_response(
    request: hyper::Request<Incoming>,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
    status: u16,
    response_body: String,
) -> std::future::Ready<io::Result<hyper::Response<Full<bytes::Bytes>>>> {
    let response = (|| {
        recorded
            .lock()
            .map_err(|_| io::Error::other("request lock"))?
            .push(RecordedRequest {
                method: request.method().to_string(),
                path: request.uri().to_string(),
                body: String::new(),
                authorization: request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
            });
        hyper::Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .header("connection", "close")
            .body(Full::new(bytes::Bytes::from(response_body)))
            .map_err(io::Error::other)
    })();
    std::future::ready(response)
}

fn page(kind: &str, items: Vec<Value>) -> Value {
    json!({"kind":kind,"items":items})
}

fn resource(location_field: &str, location: &str) -> Value {
    json!({
        "id":"18446744073709551615", "name":"runner-native", "status":"READY",
        location_field:format!("https://www.googleapis.com/compute/v1/projects/synthetic-project/{location}"),
        "labels":{"garmcontrollerid":"controller-1","private_value":"must-not-be-exported"},
        "metadata":{"items":[{"key":"bootstrap","value":"must-not-be-exported"}]}
    })
}

#[tokio::test]
async fn collects_all_billable_kinds_without_exporting_labels_or_bootstrap() {
    for kind in InventoryKind::ALL {
        let (page_kind, field, location, path_kind) = match kind {
            InventoryKind::VirtualMachine => (
                "compute#instanceList",
                "zone",
                "zones/us-central1-a",
                "instances",
            ),
            InventoryKind::Disk => ("compute#diskList", "zone", "zones/us-central1-a", "disks"),
            InventoryKind::AllocatedAddress => (
                "compute#addressList",
                "region",
                "regions/us-central1",
                "addresses",
            ),
        };
        let mut owned = resource(field, location);
        if kind == InventoryKind::VirtualMachine {
            owned["disks"] = json!([
                {"type":"PERSISTENT","source":"https://www.googleapis.com/compute/v1/projects/synthetic-project/zones/us-central1-a/disks/boot"},
                {"type":"SCRATCH"}
            ]);
        }
        let mut foreign = owned.clone();
        foreign["id"] = json!("2");
        foreign["name"] = json!("foreign");
        foreign["labels"]["garmcontrollerid"] = json!("private-foreign-tag");
        let mut untagged = owned.clone();
        untagged["id"] = json!("3");
        untagged["name"] = json!("untagged");
        untagged["labels"] = Value::Null;
        let server =
            inventory_server(vec![(200, page(page_kind, vec![owned, foreign, untagged]))]).await;
        let client = backend(&server, false).await;
        let inventory = client
            .inventory(
                kind,
                "controller-1",
                Instant::now() + Duration::from_secs(3),
            )
            .await
            .unwrap();
        assert_eq!(inventory.kind, kind);
        if kind == InventoryKind::VirtualMachine {
            assert_eq!(
                inventory.resources[0].dependencies,
                ["projects/synthetic-project/zones/us-central1-a/disks/boot"]
            );
        }
        assert_eq!(
            inventory.resources.len(),
            3,
            "inventory must include unowned/untagged resources"
        );
        assert_eq!(
            inventory.resources[0].incarnation.as_deref(),
            Some("18446744073709551615")
        );
        assert_eq!(
            inventory.resources[0].ownership,
            InventoryOwnership::Controller
        );
        assert_eq!(
            inventory.resources[1].ownership,
            InventoryOwnership::OtherController
        );
        assert_eq!(
            inventory.resources[2].ownership,
            InventoryOwnership::Untagged
        );
        let encoded = serde_json::to_string(&inventory).unwrap();
        assert!(!encoded.contains("must-not-be-exported"));
        assert!(!encoded.contains("private-foreign-tag"));
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert!(
            requests[0]
                .path
                .contains(&format!("/{path_kind}?maxResults=500"))
        );
        assert!(!requests[0].path.contains("filter="));
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer contract-token")
        );
    }
}

#[tokio::test]
async fn pagination_cannot_turn_a_failed_or_repeated_page_into_empty_success() {
    for final_status in [200, 403] {
        let mut first = page(
            "compute#diskList",
            vec![resource("zone", "zones/us-central1-a")],
        );
        first["nextPageToken"] = json!("next&not-a-query");
        let final_page = if final_status == 200 {
            first.clone()
        } else {
            json!({"error":{"message":"denied"}})
        };
        let server = inventory_server(vec![(200, first), (final_status, final_page)]).await;
        let client = backend(&server, false).await;
        assert!(
            client
                .inventory(
                    InventoryKind::Disk,
                    "controller-1",
                    Instant::now() + Duration::from_secs(3)
                )
                .await
                .is_err()
        );
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].path.contains("pageToken=next%26not-a-query"));
    }
}

#[tokio::test]
async fn empty_native_inventory_is_distinct_from_unknown_shape_or_partial_warning() {
    for response in [
        json!({"kind":"compute#addressList","warning":{"code":"NO_RESULTS_ON_PAGE"}}),
        json!({"kind":"compute#addressList","warning":{"code":"UNREACHABLE"}}),
        json!({"items":[]}),
    ] {
        let complete = response["warning"]["code"] == "NO_RESULTS_ON_PAGE";
        let server = inventory_server(vec![(200, response)]).await;
        let client = backend(&server, false).await;
        let result = client
            .inventory(
                InventoryKind::AllocatedAddress,
                "controller-1",
                Instant::now() + Duration::from_secs(3),
            )
            .await;
        assert_eq!(result.is_ok(), complete);
        if let Ok(collection) = result {
            assert!(collection.resources.is_empty());
        }
    }
}

#[tokio::test]
async fn foreign_project_same_zone_and_inexact_native_ids_are_rejected() {
    let mut foreign = resource("zone", "zones/us-central1-a");
    foreign["zone"] =
        json!("https://www.googleapis.com/compute/v1/projects/foreign-project/zones/us-central1-a");
    let mut missing_id = resource("zone", "zones/us-central1-a");
    missing_id["id"] = Value::Null;
    for row in [foreign, missing_id] {
        let server = inventory_server(vec![(200, page("compute#instanceList", vec![row]))]).await;
        let client = backend(&server, false).await;
        assert!(
            client
                .inventory(
                    InventoryKind::VirtualMachine,
                    "controller-1",
                    Instant::now() + Duration::from_secs(3)
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn address_inventory_uses_native_zone_region_instead_of_pricing_label() {
    let server = inventory_server(vec![(200, page("compute#addressList", Vec::new()))]).await;
    let mut settings = super::identity::settings(
        &server.endpoint,
        false,
        super::cloud_server::MockShape::Standard,
    );
    settings.pricing_location = "europe-west1".to_owned();
    let client = outrider::cloud::gcp::GcpBackend::with_clients(
        settings,
        reqwest::Client::new(),
        google_cloud_auth::credentials::AccessTokenCredentials::from(
            super::cloud_server::StaticCredential,
        ),
        Instant::now() + Duration::from_secs(3),
    )
    .await
    .unwrap();
    let report = client
        .inventory(
            InventoryKind::AllocatedAddress,
            "controller-1",
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .unwrap();
    assert_eq!(report.scope.location, "us-central1");
    let requests = server.requests.lock().unwrap();
    assert!(requests[0].path.contains("/regions/us-central1/addresses"));
    assert!(!requests[0].path.contains("europe-west1"));
}
