use super::identity::{backend, query, settings};
use std::time::Duration;

use google_cloud_auth::credentials::AccessTokenCredentials;
use outrider::cloud::{CloudCatalog, OwnershipScope, gcp::GcpBackend};
use sha2::{Digest, Sha256};

use super::cloud_server::{
    CHILD_CONFIG, MockRoutes, MockShape, StaticCredential, start_server, start_server_with_routes,
};

#[tokio::test]
async fn read_only_cloud_calls() {
    let server = start_server()
        .await
        .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server, true).await;
    let _ = backend
        .catalog(
            query("spot"),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"))
        .offers;
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request lock"));
    assert!(requests.iter().all(|request| request.method == "GET"
        || request.path.contains("/advice/capacity") && request.method == "POST"));
    assert!(
        requests
            .iter()
            .any(|request| request.path.contains("/advice/capacity"))
    );
}

#[tokio::test]
async fn binding_project_zone_mismatch() {
    let server = start_server()
        .await
        .unwrap_or_else(|_| panic!("mock server"));
    let mut settings = settings(&server.endpoint, false, MockShape::Standard);
    let mismatched_config = CHILD_CONFIG.replace("us-central1-a", "us-east1-b");
    std::fs::write(
        &settings.provider_configuration_path,
        mismatched_config.as_bytes(),
    )
    .unwrap_or_else(|_| panic!("write mismatched config"));
    settings.configuration_sha256 = hex::encode(Sha256::digest(mismatched_config.as_bytes()));
    let client = reqwest::Client::new();
    let result = GcpBackend::with_clients(
        settings,
        client,
        AccessTokenCredentials::from(StaticCredential),
        tokio::time::Instant::now() + Duration::from_secs(5),
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn unsupported_attachment_rejected() {
    let server = start_server()
        .await
        .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server, false).await;
    let mut request = query("spot");
    request.profile.gpu = Some(outrider::config::GpuRequirement {
        vendor: "nvidia".to_owned(),
        model: Some("A100".to_owned()),
        count: 1,
        memory_gib: Some(40),
    });
    let offers = backend
        .catalog(
            request,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"))
        .offers;
    assert!(offers.is_empty());
}

#[tokio::test]
async fn provider_ownership_observations() {
    let server = start_server()
        .await
        .unwrap_or_else(|_| panic!("mock server"));
    let gcp_backend = backend(&server, false).await;
    let found = gcp_backend
        .list(
            &OwnershipScope {
                controller_id: "controller-1".to_owned(),
                resource_name: None,
            },
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("instance list"));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].identity.resource_id, "1234567890123456789");
    assert_eq!(found[0].identity.account_id, "synthetic-project");
    assert_eq!(found[0].identity.location, "us-central1-a");
    assert_eq!(found[0].name, "runner-journal");
    assert_eq!(
        found[0]
            .identity
            .dependency_ids
            .get("instance_name")
            .map(String::as_str),
        Some("runner-native")
    );
    assert_eq!(found[0].ownership_tags.len(), 2);
    let inspected = gcp_backend
        .inspect(
            &found[0].identity,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("inspect native identity"));
    assert_eq!(
        inspected
            .as_ref()
            .map(|instance| instance.identity.resource_id.as_str()),
        Some("1234567890123456789")
    );
    for (cloud, account, location) in [
        ("aws", "synthetic-project", "us-central1-a"),
        ("gcp", "foreign-project", "us-central1-a"),
        ("gcp", "synthetic-project", "us-east1-b"),
    ] {
        let mut out_of_scope = found[0].identity.clone();
        out_of_scope.cloud = cloud.to_owned();
        out_of_scope.account_id = account.to_owned();
        out_of_scope.location = location.to_owned();
        assert!(
            gcp_backend
                .inspect(
                    &out_of_scope,
                    tokio::time::Instant::now() + Duration::from_secs(5)
                )
                .await
                .is_err()
        );
    }
    let mut absent_identity = found[0].identity.clone();
    absent_identity.resource_id = "9999999999999999999".to_owned();
    assert_eq!(
        gcp_backend
            .inspect(
                &absent_identity,
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .unwrap_or_else(|_| panic!("absent native ID")),
        None
    );
    assert!(
        gcp_backend
            .list(
                &OwnershipScope {
                    controller_id: String::new(),
                    resource_name: None
                },
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    assert!(
        gcp_backend
            .list(
                &OwnershipScope {
                    controller_id: "controller-1".to_owned(),
                    resource_name: Some(String::new())
                },
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    let foreign = gcp_backend
        .list(
            &OwnershipScope {
                controller_id: "controller-foreign".to_owned(),
                resource_name: None,
            },
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("foreign controller list"));
    assert!(foreign.is_empty());
    let wrong_name = gcp_backend
        .list(
            &OwnershipScope {
                controller_id: "controller-1".to_owned(),
                resource_name: Some("different-runner".to_owned()),
            },
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("runner-name scope"));
    assert!(wrong_name.is_empty());

    let malformed_server = start_server_with_routes(MockRoutes {
        shape: MockShape::Standard,
        advice_status: 200,
        advice_body: r#"{"recommendations":[]}"#.to_owned(),
        instances_status: 200,
        instances_body: r#"{"items":[{"id":"1234567890123456789","name":"runner-native","status":"RUNNING","zone":"https://www.googleapis.com/compute/v1/projects/synthetic-project/zones/us-central1-a","labels":{"garmcontrollerid":"controller-1"}}]}"#.to_owned(),
    }).await.unwrap_or_else(|_| panic!("malformed ownership server"));
    let malformed_backend = backend(&malformed_server, false).await;
    assert!(
        malformed_backend
            .list(
                &OwnershipScope {
                    controller_id: "controller-1".to_owned(),
                    resource_name: None
                },
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );

    let other_controller_server = start_server_with_routes(MockRoutes {
        shape: MockShape::Standard,
        advice_status: 200,
        advice_body: r#"{"recommendations":[]}"#.to_owned(),
        instances_status: 200,
        instances_body: r#"{"items":[{"id":"1234567890123456789","name":"runner-native","status":"RUNNING","zone":"https://www.googleapis.com/compute/v1/projects/synthetic-project/zones/us-central1-a","labels":{"garmcontrollerid":"controller-other","garmpoolid":"pool-1"},"metadata":{"items":[{"key":"runner_name","value":"runner-journal"}]}}]}"#.to_owned(),
    }).await.unwrap_or_else(|_| panic!("known foreign-owned instance server"));
    let other_controller_backend = backend(&other_controller_server, false).await;
    assert!(
        other_controller_backend
            .inspect(
                &found[0].identity,
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );

    let foreign_project_server = start_server_with_routes(MockRoutes {
        shape: MockShape::Standard,
        advice_status: 200,
        advice_body: r#"{"recommendations":[]}"#.to_owned(),
        instances_status: 200,
        instances_body: r#"{"items":[{"id":"1234567890123456789","name":"runner-native","status":"RUNNING","zone":"https://www.googleapis.com/compute/v1/projects/foreign-project/zones/us-central1-a","labels":{"garmcontrollerid":"controller-1","garmpoolid":"pool-1"},"metadata":{"items":[{"key":"runner_name","value":"runner-journal"}]}}]}"#.to_owned(),
    }).await.unwrap_or_else(|_| panic!("foreign project server"));
    let foreign_project_backend = backend(&foreign_project_server, false).await;
    assert!(
        foreign_project_backend
            .list(
                &OwnershipScope {
                    controller_id: "controller-1".to_owned(),
                    resource_name: None
                },
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );

    let outage_server = start_server_with_routes(MockRoutes {
        shape: MockShape::Standard,
        advice_status: 200,
        advice_body: r#"{"recommendations":[]}"#.to_owned(),
        instances_status: 503,
        instances_body: r#"{"error":"unavailable"}"#.to_owned(),
    })
    .await
    .unwrap_or_else(|_| panic!("instance outage server"));
    let outage_backend = backend(&outage_server, false).await;
    assert!(
        outage_backend
            .list(
                &OwnershipScope {
                    controller_id: "controller-1".to_owned(),
                    resource_name: None
                },
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
}
