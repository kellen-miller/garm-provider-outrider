use std::{collections::BTreeSet, time::Duration};

use outrider::cloud::CloudCatalog;
use rust_decimal::Decimal;
use tokio::time::Instant;

use super::cloud_server::{
    MockResponse, backend, changed_retail_page, image_response, irrelevant_retail_page, profile,
    query, retail_response, sku, start_server,
};

#[tokio::test]
async fn broader_discovered_scope_refreshes_and_then_serves_subset_quotes() {
    let server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku("Standard_D2s_v3"),sku("Standard_D2as_v5")]})
                .to_string(),
        },
        retail_response(),
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku("Standard_D2s_v3"),sku("Standard_D2as_v5")]})
                .to_string(),
        },
        changed_retail_page("0.2"),
        irrelevant_retail_page(),
        image_response(),
    ])
    .await
    .unwrap_or_else(|_| panic!("scoped pricing server"));
    let backend = backend(&server).await;
    let mut profile = profile();
    profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    profile
        .administrator_limits
        .allowed_machine_types
        .remove("azure");
    let first = backend
        .catalog(
            query(profile.clone()),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("exact SKU catalog"));
    profile.machine_types.remove("azure");
    let second = backend
        .catalog(query(profile), Instant::now() + Duration::from_secs(5))
        .await
        .unwrap_or_else(|error| panic!("all SKU catalog: {error}"));
    assert_eq!(first.offers.len(), 1);
    assert_eq!(second.offers.len(), 1);
    let cached_quote = backend
        .quote(
            &second.offers[0],
            Duration::from_secs(3600),
            Duration::from_secs(600),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("subset price quote"));
    assert_eq!(cached_quote.compute.hourly_rate, Some(Decimal::new(2, 1)));
    assert_eq!(
        second.offers[0]
            .candidate
            .price
            .as_ref()
            .unwrap_or_else(|| panic!("refreshed price"))
            .compute
            .hourly_rate,
        Some(Decimal::new(2, 1))
    );
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request lock"));
    let retail = requests
        .iter()
        .filter(|request| request.path.starts_with("/api/retail/prices"))
        .collect::<Vec<_>>();
    assert_eq!(retail.len(), 3);
    let first_filter =
        urlencoding::decode(&retail[0].path).unwrap_or_else(|_| panic!("first filter"));
    let second_filter =
        urlencoding::decode(&retail[1].path).unwrap_or_else(|_| panic!("second filter"));
    assert!(first_filter.contains("armSkuName eq 'Standard_D2s_v3'"));
    assert!(second_filter.contains("armSkuName eq 'Standard_D2s_v3'"));
    assert!(second_filter.contains("armSkuName eq 'Standard_D2as_v5'"));
}

#[tokio::test]
async fn a_machine_name_cannot_inject_odata_or_query_parameters() {
    let name = "Standard_D2s_v3' or armRegionName eq 'westus&foo=bar";
    let server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku(name)]}).to_string(),
        },
        retail_response(),
    ])
    .await
    .unwrap_or_else(|_| panic!("filter server"));
    let backend = backend(&server).await;
    let mut profile = profile();
    profile
        .machine_types
        .insert("azure".to_owned(), vec![name.to_owned()]);
    profile
        .administrator_limits
        .allowed_machine_types
        .remove("azure");
    let discovery = backend
        .catalog(query(profile), Instant::now() + Duration::from_secs(5))
        .await
        .unwrap_or_else(|_| panic!("escaped filter catalog"));
    assert!(discovery.offers.is_empty());
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request lock"));
    let path = &requests
        .last()
        .unwrap_or_else(|| panic!("retail request"))
        .path;
    let url = reqwest::Url::parse(&format!("http://localhost{path}"))
        .unwrap_or_else(|_| panic!("retail URL"));
    assert_eq!(url.query_pairs().count(), 3);
    let filter = url
        .query_pairs()
        .find(|(name, _)| name == "$filter")
        .unwrap_or_else(|| panic!("single filter"))
        .1;
    assert!(
        filter.contains("armSkuName eq 'Standard_D2s_v3'' or armRegionName eq ''westus&foo=bar'")
    );
}
