use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use outrider::cloud::{CloudCatalog, CloudError};
use rust_decimal::Decimal;

use super::cloud_server::{
    MockResponse, backend, changed_retail_page, image_response, irrelevant_retail_page,
    large_retail_page, profile, query, retail_response, sku, start_server,
};

#[tokio::test]
async fn retail_snapshot_bounds_multi_offer_paging_and_keeps_exact_prices() {
    let server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
        },
        large_retail_page(),
        irrelevant_retail_page(),
        image_response(),
        image_response(),
    ])
    .await
    .unwrap_or_else(|_| panic!("snapshot mock server"));
    let backend = backend(&server).await;
    let mut request_profile = profile();
    request_profile.purchasing_models = BTreeSet::from(["on_demand".to_owned(), "spot".to_owned()]);
    let offers = backend
        .catalog(
            query(request_profile.clone()),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog offers"))
        .offers;
    assert_eq!(offers.len(), 2);
    let source_observation = offers[0]
        .candidate
        .price
        .as_ref()
        .unwrap_or_else(|| panic!("catalog price"))
        .compute
        .observed_at_unix_seconds;
    let mut quotes = Vec::new();
    for (index, offer) in offers.iter().enumerate() {
        if index == 1 {
            tokio::time::sleep(Duration::from_millis(1050)).await;
        }
        let quote = backend
            .quote(
                offer,
                Duration::from_secs(3600),
                Duration::from_secs(600),
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap_or_else(|_| panic!("cached retail quote"));
        let expected = if offer.candidate.purchasing == "spot" {
            Decimal::new(3, 2)
        } else {
            Decimal::new(10, 2)
        };
        assert_eq!(quote.compute.hourly_rate, Some(expected));
        assert_eq!(quote.compute.observed_at_unix_seconds, source_observation);
        quotes.push(quote);
    }
    let mut candidates = offers
        .iter()
        .map(|offer| offer.candidate.clone())
        .collect::<Vec<_>>();
    for (candidate, quote) in candidates.iter_mut().zip(quotes) {
        candidate.price = Some(quote);
        candidate
            .price
            .as_mut()
            .unwrap_or_else(|| panic!("ranked price"))
            .refresh_observed_age();
    }
    assert!(
        candidates[0]
            .price
            .as_ref()
            .unwrap()
            .observed_age_seconds
            .unwrap_or_default()
            >= 1
    );
    let strict_policy = BTreeMap::from([(
        "strict".to_owned(),
        outrider::policy::PolicyDefinition {
            strategy: outrider::policy::Policy::Economy {
                max_price_age_seconds: 0,
            },
            missing_data: outrider::policy::MissingDataBehavior::Reject,
        },
    )]);
    assert!(
        outrider::policy::rank_configured("strict", &strict_policy, &candidates, &request_profile)
            .is_err()
    );
    server
        .task
        .await
        .unwrap_or_else(|_| panic!("snapshot server task"))
        .unwrap_or_else(|_| panic!("snapshot server"));
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request capture"));
    let retail = requests
        .iter()
        .filter(|request| request.path.starts_with("/api/retail/prices"))
        .collect::<Vec<_>>();
    assert_eq!(retail.len(), 2);
    assert!(
        retail[0].path.contains(
            "$filter=(serviceName%20eq%20%27Virtual%20Machines%27%20or%20serviceName%20eq%20%27Storage%27)%20and%20armRegionName%20eq%20%27eastus%27"
        ),
        "unexpected Retail filter: {}",
        retail[0].path
    );
    assert!(!retail[0].path.contains("Virtual%20Network"));
}

#[tokio::test]
async fn strict_nonzero_price_age_rejects_partial_refresh() {
    let server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
        },
        retail_response(),
        image_response(),
        large_retail_page(),
        MockResponse {
            status: 503,
            body: "{}".to_owned(),
        },
    ])
    .await
    .unwrap_or_else(|_| panic!("refresh mock server"));
    let backend = backend(&server).await;
    let mut profile = profile();
    profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    let offers = backend
        .catalog(
            query(profile),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("initial catalog"))
        .offers;
    tokio::time::sleep(Duration::from_millis(1050)).await;
    let result = backend
        .quote(
            offers.first().unwrap_or_else(|| panic!("offer")),
            Duration::from_secs(3600),
            Duration::from_secs(1),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await;
    assert!(matches!(result, Err(CloudError::PriceUnavailable(_))));
    server
        .task
        .await
        .unwrap_or_else(|_| panic!("refresh server task"))
        .unwrap_or_else(|_| panic!("refresh server"));
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request capture"));
    let retail_count = requests
        .iter()
        .filter(|request| request.path.starts_with("/api/retail/prices"))
        .count();
    assert_eq!(
        retail_count, 3,
        "strict-age refresh followed the partial page"
    );
    assert_eq!(
        requests.len(),
        6,
        "catalog and quote both revalidated the image"
    );
}

#[tokio::test]
async fn strict_nonzero_price_age_refreshes_snapshot_before_quote() {
    let server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
        },
        retail_response(),
        image_response(),
        changed_retail_page("0.11"),
        irrelevant_retail_page(),
    ])
    .await
    .unwrap_or_else(|_| panic!("strict refresh mock server"));
    let backend = backend(&server).await;
    let mut profile = profile();
    profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    let offers = backend
        .catalog(
            query(profile),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("initial catalog"))
        .offers;
    tokio::time::sleep(Duration::from_millis(1050)).await;
    let quote = backend
        .quote(
            offers.first().unwrap_or_else(|| panic!("offer")),
            Duration::from_secs(3600),
            Duration::from_secs(1),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("strict-age refresh quote"));
    assert_eq!(quote.compute.hourly_rate, Some(Decimal::new(11, 2)));
    server
        .task
        .await
        .unwrap_or_else(|_| panic!("strict refresh server task"))
        .unwrap_or_else(|_| panic!("strict refresh server"));
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request capture"));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path.starts_with("/api/retail/prices"))
            .count(),
        3,
        "strict-age refresh fetches both Retail pages"
    );
}

#[tokio::test]
async fn zero_price_age_forces_refresh_before_quote() {
    let server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
        },
        retail_response(),
        image_response(),
        changed_retail_page("0.11"),
        irrelevant_retail_page(),
    ])
    .await
    .unwrap_or_else(|_| panic!("zero-age refresh mock server"));
    let backend = backend(&server).await;
    let mut profile = profile();
    profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    let offers = backend
        .catalog(
            query(profile),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("initial zero-age catalog"))
        .offers;
    let quote = backend
        .quote(
            offers.first().unwrap_or_else(|| panic!("offer")),
            Duration::from_secs(3600),
            Duration::ZERO,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("zero-age refresh quote"));
    assert_eq!(quote.compute.hourly_rate, Some(Decimal::new(11, 2)));
    server
        .task
        .await
        .unwrap_or_else(|_| panic!("zero-age refresh server task"))
        .unwrap_or_else(|_| panic!("zero-age refresh server"));
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request capture"));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path.starts_with("/api/retail/prices"))
            .count(),
        3,
        "zero-age request bypasses cached Retail pages and fetches a new snapshot"
    );
}

#[tokio::test]
async fn sku_price_pagination() {
    let sku_path =
        "/subscriptions/11111111-1111-4111-8111-111111111111/providers/Microsoft.Compute/skus";
    let server = start_server(vec![image_response(),
        MockResponse { status: 200, body: r#"{"value":[],"nextLink":"{{endpoint}}/subscriptions/11111111-1111-4111-8111-111111111111/providers/Microsoft.Compute/skus?api-version=2021-07-01&$skiptoken=2"}"#.to_owned() },
        MockResponse { status: 200, body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string() },
        MockResponse { status: 200, body: r#"{"Items":[],"NextPageLink":"{{endpoint}}/api/retail/prices?api-version=2023-01-01-preview&$skip=2"}"#.to_owned() },
        retail_response(),
    ]).await.unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server).await;
    let mut profile = profile();
    profile.purchasing_models = BTreeSet::from(["on_demand".to_owned(), "spot".to_owned()]);
    let offers = backend
        .catalog(
            query(profile),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"))
        .offers;
    assert_eq!(offers.len(), 2);
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
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path.starts_with(sku_path))
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.path.starts_with("/api/retail/prices"))
            .count(),
        2
    );
}

#[tokio::test]
async fn provider_launch_defaults_and_quote() {
    let server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
        },
        retail_response(),
        image_response(),
        image_response(),
        retail_response(),
    ])
    .await
    .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server).await;
    let mut profile = profile();
    profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    profile.administrator_limits.max_boot_disk_gib = 64;
    let offers = backend
        .catalog(
            query(profile),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"))
        .offers;
    assert_eq!(offers.len(), 1);
    let offer = &offers[0];
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("requests lock"))
            .len(),
        3,
        "catalog request count"
    );
    assert_eq!(offer.selection.binding_id, "azure-child");
    assert_eq!(offer.selection.approved_extras["disk_size_gb"], 40);
    assert_eq!(
        offer.selection.approved_extras["storage_account_type"],
        "Standard_LRS"
    );
    assert_eq!(offer.selection.approved_extras["allocate_public_ip"], false);
    assert_eq!(offer.candidate.vcpu, 8);
    assert_eq!(
        offer.candidate.boot_disk_throughput_mib_s,
        Decimal::new(57_220_458_984_375, 12)
    );
    let outrider::policy::ProviderLaunchFacts::Azure(facts) = &offer.launch_facts else {
        panic!("Azure catalog emits Azure launch facts");
    };
    assert_eq!(facts.minimum_os_disk_gib, 30);
    assert_eq!(
        facts.minimum_os_disk_source,
        "operator verified Canonical image documentation"
    );
    assert_eq!(
        offer
            .candidate
            .price
            .as_ref()
            .unwrap_or_else(|| panic!("quoted compute price is present"))
            .compute
            .hourly_rate,
        Some(Decimal::new(10, 2))
    );
    assert_eq!(
        offer
            .candidate
            .price
            .as_ref()
            .unwrap_or_else(|| panic!("quoted disk price is present"))
            .disk
            .hourly_rate,
        Some(Decimal::new(3, 0) / Decimal::from(730))
    );
    let mut tampered = offer.clone();
    tampered
        .selection
        .approved_extras
        .insert("spot".to_owned(), serde_json::json!(true));
    assert!(
        backend
            .quote(
                &tampered,
                Duration::from_secs(3600),
                Duration::from_secs(600),
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    tampered = offer.clone();
    tampered
        .selection
        .approved_extras
        .insert("disk_size_gb".to_owned(), serde_json::json!(64));
    assert!(
        backend
            .quote(
                &tampered,
                Duration::from_secs(3600),
                Duration::from_secs(600),
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    tampered = offer.clone();
    tampered.selection.approved_extras.insert(
        "storage_account_type".to_owned(),
        serde_json::json!("Premium_LRS"),
    );
    assert!(
        backend
            .quote(
                &tampered,
                Duration::from_secs(3600),
                Duration::from_secs(600),
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    tampered = offer.clone();
    tampered
        .selection
        .approved_extras
        .insert("allocate_public_ip".to_owned(), serde_json::json!(true));
    assert!(
        backend
            .quote(
                &tampered,
                Duration::from_secs(3600),
                Duration::from_secs(600),
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    tampered = offer.clone();
    let outrider::policy::ProviderLaunchFacts::Azure(tampered_facts) = &mut tampered.launch_facts
    else {
        panic!("Azure launch facts");
    };
    tampered_facts.minimum_os_disk_gib = 31;
    assert!(
        backend
            .quote(
                &tampered,
                Duration::from_secs(3600),
                Duration::from_secs(600),
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("requests lock"))
            .len(),
        3,
        "minimum-size tampering rejects before I/O"
    );
    tampered = offer.clone();
    let outrider::policy::ProviderLaunchFacts::Azure(tampered_facts) = &mut tampered.launch_facts
    else {
        panic!("Azure launch facts");
    };
    assert_eq!(tampered_facts.image_version, "22.04.202601010");
    tampered_facts.image_version = "22.04.202601011".to_owned();
    assert_eq!(tampered_facts.image_version, "22.04.202601011");
    assert!(
        backend
            .quote(
                &tampered,
                Duration::from_secs(3600),
                Duration::from_secs(600),
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    let paths: Vec<_> = {
        let requests = server
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("requests lock"));
        requests
            .iter()
            .map(|request| request.path.clone())
            .collect()
    };
    assert_eq!(
        paths.len(),
        4,
        "image-version mismatch reaches only ARM: {paths:?}"
    );
    tampered = offer.clone();
    tampered.candidate.location = "westus".to_owned();
    assert!(
        backend
            .quote(
                &tampered,
                Duration::from_secs(3600),
                Duration::from_secs(600),
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    let quote = backend
        .quote(
            offer,
            Duration::from_secs(3600),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|error| {
            let requests = server
                .requests
                .lock()
                .unwrap_or_else(|_| panic!("requests lock"));
            let paths: Vec<_> = requests
                .iter()
                .map(|request| request.path.clone())
                .collect();
            panic!("quote: {error:?}; requests: {paths:?}");
        });
    assert_eq!(quote.compute.hourly_rate, Some(Decimal::new(10, 2)));
}
