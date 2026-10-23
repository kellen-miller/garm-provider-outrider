use super::identity::{assert_quote_rejected, backend, backend_for_shape, query};
use std::{collections::BTreeSet, time::Duration};

use super::cloud_server::{MockRoutes, MockShape, start_server, start_server_with_routes};
use outrider::{
    cloud::CloudCatalog,
    policy::{Policy, ProviderLaunchFacts, rank},
};
use rust_decimal::Decimal;
#[tokio::test]
async fn catalog_enforces_profile_account_and_location_without_query_scopes() {
    let server = start_server()
        .await
        .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server, false).await;
    let mut wrong_account = query("spot");
    wrong_account.accounts.clear();
    wrong_account.profile.accounts = vec!["other-project".to_owned()];
    assert!(
        backend
            .catalog(
                wrong_account,
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .unwrap_or_default()
            .offers
            .is_empty()
    );
    let mut wrong_location = query("spot");
    wrong_location.locations.clear();
    wrong_location.profile.locations = vec!["us-east1".to_owned()];
    assert!(
        backend
            .catalog(
                wrong_location,
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .unwrap_or_default()
            .offers
            .is_empty()
    );
}

#[tokio::test]
async fn machine_price_pagination() {
    let server = start_server()
        .await
        .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server, false).await;
    let result = backend
        .catalog(
            query("spot"),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"))
        .offers;
    assert_eq!(result.len(), 1);
    let quote = backend
        .quote(
            &result[0],
            Duration::from_secs(3600),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|error| {
            let paths = server
                .requests
                .lock()
                .map(|requests| {
                    requests
                        .iter()
                        .map(|request| request.path.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            panic!("Cloud Billing quote: {error}; paths={paths:?}")
        });
    assert_eq!(quote.compute.hourly_rate, Some(Decimal::new(12, 2)));
    assert_eq!(
        quote.disk.hourly_rate,
        Some(Decimal::new(4, 3) / Decimal::from(730u32))
    );
    assert_eq!(quote.observed_age_seconds, Some(0));
    let mut policy_candidate = result[0].candidate.clone();
    policy_candidate.price = Some(quote.clone());
    let policy = Policy::Economy {
        max_price_age_seconds: 60,
    };
    let ranking = rank(&policy, &[policy_candidate], &query("spot").profile)
        .unwrap_or_else(|error| panic!("GCP quote policy filtering: {error}"));
    assert_eq!(ranking.len(), 1);
    assert!(ranking[0].eligible);
    assert!(quote.compute.source.contains("CPU-N2-SPOT"));
    assert!(quote.compute.source.contains("RAM-N2-SPOT"));
    assert!(!quote.compute.source.contains("CPU-N2D-SPOT"));
    assert_eq!(
        quote.provider_request_ids,
        vec!["request-contract-1", "request-contract-1"]
    );
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request lock"));
    assert!(
        requests
            .iter()
            .any(|request| request.path.contains("pageToken=second%2Fpage"))
    );
    assert!(
        requests
            .iter()
            .any(|request| request.path.contains("pageToken=pricing%2Fpage"))
    );
    assert!(
        requests
            .iter()
            .all(|request| request.authorization.as_deref() == Some("Bearer contract-token"))
    );
}

#[tokio::test]
async fn provider_launch_defaults_and_quote() {
    let server = start_server()
        .await
        .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server, false).await;
    let offers = backend
        .catalog(
            query("spot"),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"))
        .offers;
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].selection.binding_id, "gcp-child");
    assert_eq!(offers[0].selection.flavor, "n2-standard-4");
    assert_eq!(offers[0].candidate.location, "us-central1");
    assert_eq!(offers[0].selection.approved_extras["disksize"], 40);
    assert_eq!(offers[0].selection.approved_extras["spot"], true);
    assert!(matches!(
        offers[0].launch_facts,
        ProviderLaunchFacts::Gcp(_)
    ));
    let quote = backend
        .quote(
            &offers[0],
            Duration::from_secs(30),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|error| {
            let paths = server
                .requests
                .lock()
                .map(|requests| {
                    requests
                        .iter()
                        .map(|request| request.path.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            panic!("revalidated quote: {error}; paths={paths:?}")
        });
    assert_eq!(quote.compute.minimum_commitment_seconds, 60);
    assert_eq!(quote.disk.minimum_commitment_seconds, 0);
    let mut tampered = offers[0].clone();
    tampered
        .selection
        .approved_extras
        .insert("spot".to_owned(), serde_json::json!(false));
    assert!(
        backend
            .quote(
                &tampered,
                Duration::from_secs(60),
                Duration::from_secs(600),
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn quote_rejects_stale_pins_and_image_minimum_disk_tampering() {
    let server = start_server()
        .await
        .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server, false).await;
    let offers = backend
        .catalog(
            query("spot"),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"))
        .offers;

    let mut wrong_revision = offers[0].clone();
    wrong_revision.selection.source_revision.push('x');
    assert_quote_rejected(&backend, &wrong_revision).await;
    let mut wrong_binary = offers[0].clone();
    wrong_binary.selection.binary_sha256 = "0".repeat(64);
    assert_quote_rejected(&backend, &wrong_binary).await;
    let mut wrong_configuration = offers[0].clone();
    wrong_configuration.selection.configuration_sha256 = "0".repeat(64);
    assert_quote_rejected(&backend, &wrong_configuration).await;

    let mut undersized_disk = offers[0].clone();
    undersized_disk.candidate.boot_disk_gib = 10;
    undersized_disk
        .selection
        .approved_extras
        .insert("disksize".to_owned(), serde_json::json!(10));
    assert_quote_rejected(&backend, &undersized_disk).await;
}

#[tokio::test]
async fn windows_license_and_gpu_disks() {
    let server = start_server_with_routes(MockRoutes {
        shape: MockShape::WindowsSql,
        advice_status: 200,
        advice_body: r#"{"recommendations":[]}"#.to_owned(),
        instances_status: 200,
        instances_body: r#"{"items":[]}"#.to_owned(),
    })
    .await
    .unwrap_or_else(|_| panic!("Windows contract server"));
    let backend = backend_for_shape(&server, false, MockShape::WindowsSql).await;
    let mut windows = query("spot");
    windows.profile.os = "windows".to_owned();
    windows.profile.image = "windows-synthetic".to_owned();
    windows.profile.administrator_limits.allowed_os = BTreeSet::from(["windows".to_owned()]);
    windows.profile.administrator_limits.allowed_images =
        BTreeSet::from(["windows-synthetic".to_owned()]);
    windows
        .profile
        .administrator_limits
        .allowed_machine_types
        .insert(
            "gcp".to_owned(),
            BTreeSet::from(["n2-standard-2".to_owned()]),
        );
    let windows_offers = backend
        .catalog(
            windows,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("Windows catalog"))
        .offers;
    assert_eq!(windows_offers.len(), 1);
    let quote = backend
        .quote(
            &windows_offers[0],
            Duration::from_secs(3600),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|error| panic!("Windows and SQL quote: {error}"));
    assert_eq!(quote.compute.hourly_rate, Some(Decimal::new(6, 2)));
    assert_eq!(quote.licensing.hourly_rate, Some(Decimal::new(16, 2)));
}
