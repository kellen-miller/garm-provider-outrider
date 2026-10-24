use std::time::Duration;

use outrider::cloud::{CloudCatalog, CloudError};
use tokio::time::Instant;

use super::cloud_server::{
    MockResponse, MockRoute, backend, image_response, irrelevant_retail_page, large_retail_page,
    premium_backend, profile, query, sku, start_server_with_headers, start_server_with_routes,
};

#[tokio::test]
async fn throttled_price_page_retries_same_page_after_server_delay() {
    let server = start_server_with_headers(vec![
        (image_response(), vec![]),
        (
            MockResponse {
                status: 200,
                body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
            },
            vec![],
        ),
        (large_retail_page(), vec![]),
        (
            MockResponse {
                status: 429,
                body: "throttle".to_owned(),
            },
            vec![("Retry-After".to_owned(), "1".to_owned())],
        ),
        (irrelevant_retail_page(), vec![]),
        (image_response(), vec![]),
    ])
    .await
    .unwrap_or_else(|_| panic!("mock server"));
    let backend = premium_backend(&server).await;
    let started = Instant::now();
    let discovery = backend
        .catalog(query(profile()), started + Duration::from_secs(5))
        .await
        .unwrap_or_else(|error| panic!("complete catalog: {error}"));
    assert!(!discovery.offers.is_empty());
    assert!(started.elapsed() >= Duration::from_secs(1));
    let mut quote = backend
        .quote(
            &discovery.offers[0],
            Duration::from_secs(3600),
            Duration::from_secs(600),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("cached quote"));
    quote.refresh_observed_age();
    assert!(quote.observed_age_seconds.is_some_and(|age| age >= 1));
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request lock"));
    let retail = requests
        .iter()
        .filter(|request| request.path.starts_with("/api/retail/prices"))
        .collect::<Vec<_>>();
    assert_eq!(retail.len(), 3);
    assert_ne!(retail[0].path, retail[1].path);
    assert_eq!(retail[1].path, retail[2].path);
    assert!(
        retail
            .iter()
            .all(|request| request.method == "GET" && request.authorization.is_none())
    );
}

#[tokio::test]
async fn concurrent_catalogs_share_a_throttled_price_refresh() {
    let sku_response = || MockResponse {
        status: 200,
        body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
    };
    let server = start_server_with_routes(vec![
        MockRoute {
            path_contains: Some("/publishers/".to_owned()),
            response: image_response(),
            headers: vec![],
        },
        MockRoute {
            path_contains: Some("/publishers/".to_owned()),
            response: image_response(),
            headers: vec![],
        },
        MockRoute {
            path_contains: Some("/skus?".to_owned()),
            response: sku_response(),
            headers: vec![],
        },
        MockRoute {
            path_contains: Some("/skus?".to_owned()),
            response: sku_response(),
            headers: vec![],
        },
        MockRoute {
            path_contains: Some("/api/retail/prices".to_owned()),
            response: MockResponse {
                status: 429,
                body: "throttle".to_owned(),
            },
            headers: vec![("Retry-After".to_owned(), "1".to_owned())],
        },
        MockRoute {
            path_contains: Some("/api/retail/prices".to_owned()),
            response: super::cloud_server::retail_response(),
            headers: vec![],
        },
    ])
    .await
    .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let (first, second) = tokio::join!(
        backend.catalog(query(profile()), deadline),
        backend.catalog(query(profile()), deadline),
    );
    let first = first.unwrap_or_else(|_| panic!("first catalog"));
    let second = second.unwrap_or_else(|_| panic!("shared second catalog"));
    assert!(!first.offers.is_empty() && !second.offers.is_empty());
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request lock"));
    let retail = requests
        .iter()
        .filter(|request| request.path.starts_with("/api/retail/prices"))
        .collect::<Vec<_>>();
    assert_eq!(retail.len(), 2);
    assert_eq!(retail[0].path, retail[1].path);
}

#[tokio::test]
async fn throttle_delay_cannot_extend_analysis_deadline_or_accept_partial_prices() {
    let server = start_server_with_headers(vec![
        (image_response(), vec![]),
        (
            MockResponse {
                status: 200,
                body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
            },
            vec![],
        ),
        (large_retail_page(), vec![]),
        (
            MockResponse {
                status: 429,
                body: "throttle".to_owned(),
            },
            vec![("Retry-After".to_owned(), "60".to_owned())],
        ),
    ])
    .await
    .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server).await;
    let result = backend
        .catalog(query(profile()), Instant::now() + Duration::from_secs(5))
        .await;
    assert!(matches!(result, Err(CloudError::Failure(error)) if error.kind == "timeout"));
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("request lock"))
            .len(),
        4
    );
}

#[tokio::test]
async fn repeated_throttling_has_a_fixed_retry_bound() {
    let mut responses = vec![
        (image_response(), vec![]),
        (
            MockResponse {
                status: 200,
                body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
            },
            vec![],
        ),
    ];
    for _ in 0..4 {
        responses.push((
            MockResponse {
                status: 429,
                body: "throttle".to_owned(),
            },
            vec![("Retry-After".to_owned(), "1".to_owned())],
        ));
    }

    let server = start_server_with_headers(responses)
        .await
        .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server).await;
    let result = backend
        .catalog(query(profile()), Instant::now() + Duration::from_secs(10))
        .await;
    assert!(matches!(result, Err(CloudError::Failure(error)) if error.kind == "throttled"));
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("request lock"))
            .len(),
        6
    );
}

#[tokio::test]
async fn throttle_wait_cannot_reset_price_age_or_publish_an_expired_snapshot() {
    let server = start_server_with_headers(vec![
        (image_response(), vec![]),
        (
            MockResponse {
                status: 200,
                body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
            },
            vec![],
        ),
        (
            MockResponse {
                status: 429,
                body: "throttle".to_owned(),
            },
            vec![("Retry-After".to_owned(), "1".to_owned())],
        ),
        (super::cloud_server::retail_response(), vec![]),
    ])
    .await
    .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server).await;
    let mut profile = profile();
    profile.max_price_age_seconds = 1;
    let result = backend
        .catalog(query(profile), Instant::now() + Duration::from_secs(5))
        .await;
    assert!(matches!(result, Err(CloudError::Failure(error)) if error.kind == "stale"));
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("request lock"))
            .len(),
        4
    );
}

#[tokio::test]
async fn invalid_throttle_headers_never_start_a_retry() {
    for (header, expected_kind) in [
        (None, "throttled"),
        (Some("invalid"), "throttled"),
        (Some("18446744073709551615"), "timeout"),
    ] {
        let headers = header.map_or_else(Vec::new, |value| {
            vec![("Retry-After".to_owned(), value.to_owned())]
        });
        let server = start_server_with_headers(vec![
            (image_response(), vec![]),
            (
                MockResponse {
                    status: 200,
                    body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
                },
                vec![],
            ),
            (
                MockResponse {
                    status: 429,
                    body: "throttle".to_owned(),
                },
                headers,
            ),
        ])
        .await
        .unwrap_or_else(|_| panic!("mock server"));
        let backend = backend(&server).await;
        let result = backend
            .catalog(query(profile()), Instant::now() + Duration::from_secs(5))
            .await;
        assert!(matches!(result, Err(CloudError::Failure(error)) if error.kind == expected_kind));
        assert_eq!(
            server
                .requests
                .lock()
                .unwrap_or_else(|_| panic!("request lock"))
                .len(),
            3
        );
    }
}
