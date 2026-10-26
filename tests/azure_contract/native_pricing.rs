use std::{collections::BTreeSet, sync::Arc, time::Duration};

use outrider::cloud::{CloudCatalog, azure::AzureBackend};
use rust_decimal::Decimal;
use tokio::time::Instant;

use super::cloud_server::{
    MockResponse, TestCredential, child_config, image_response, profile, query, settings,
    start_server,
};

#[tokio::test]
async fn native_disk_meters_and_unpriced_io_fail_closed() {
    for (storage_sku, duplicate_base_meter, purchasing) in [
        ("Premium_LRS", false, "spot"),
        ("Premium_LRS", false, "on_demand"),
        ("Premium_LRS", true, "spot"),
        ("StandardSSD_LRS", false, "spot"),
        ("Standard_LRS", false, "spot"),
    ] {
        let mut retail: serde_json::Value =
            serde_json::from_str(include_str!("native-retail-prices.json"))
                .unwrap_or_else(|_| panic!("native public price fixture"));
        if duplicate_base_meter {
            let mut duplicate = retail["Items"]
                .as_array()
                .and_then(|items| {
                    items
                        .iter()
                        .find(|item| item["meterName"] == "P10 LRS Disk")
                })
                .unwrap_or_else(|| panic!("native base disk meter"))
                .clone();
            duplicate["meterId"] = serde_json::json!("duplicate-base-disk");
            retail["Items"]
                .as_array_mut()
                .unwrap_or_else(|| panic!("native price items"))
                .push(duplicate);
        }

        let sku: serde_json::Value = serde_json::from_str(include_str!("native-sku.json"))
            .unwrap_or_else(|_| panic!("native SKU fixture"));
        let server = start_server(vec![
            image_response(),
            MockResponse {
                status: 200,
                body: serde_json::json!({"value":[sku]}).to_string(),
            },
            MockResponse {
                status: 200,
                body: retail.to_string(),
            },
        ])
        .await
        .unwrap_or_else(|_| panic!("native fixture server"));
        let path = std::env::temp_dir().join(format!(
            "outrider-native-azure-price-{}-{storage_sku}-{duplicate_base_meter}.toml",
            std::process::id()
        ));
        std::fs::write(&path, child_config())
            .unwrap_or_else(|_| panic!("synthetic child configuration"));
        let mut settings = settings(&server.endpoint, path.clone());
        settings
            .storage_classes
            .insert("standard".to_owned(), storage_sku.to_owned());
        settings
            .locations
            .get_mut("eastus")
            .unwrap_or_else(|| panic!("location"))
            .public_ip_sku = Some("Standard".to_owned());
        let backend = AzureBackend::with_clients(
            settings,
            reqwest::Client::new(),
            Arc::new(TestCredential),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("native price backend"));
        std::fs::remove_file(path).unwrap_or_else(|_| panic!("remove synthetic configuration"));
        let mut profile = profile();
        profile.vcpu_min = 2;
        profile.vcpu_max = 2;
        profile.memory_gib_min = 8;
        profile.memory_gib_max = 8;
        profile.boot_disk.size_gib = 100;
        profile
            .machine_types
            .insert("azure".to_owned(), vec!["Standard_D2as_v5".to_owned()]);
        profile.administrator_limits.allowed_machine_types.insert(
            "azure".to_owned(),
            BTreeSet::from(["Standard_D2as_v5".to_owned()]),
        );
        profile.purchasing_models = BTreeSet::from([purchasing.to_owned()]);
        let discovery = backend
            .catalog(query(profile), Instant::now() + Duration::from_secs(5))
            .await
            .unwrap_or_else(|error| panic!("native pricing catalog: {error}"));
        if duplicate_base_meter {
            assert!(discovery.offers.is_empty());
            assert!(
                discovery
                    .exclusions
                    .iter()
                    .any(|item| item.reason
                        == "required Azure price meters are missing or ambiguous")
            );
        } else if storage_sku == "Premium_LRS" {
            assert_eq!(discovery.offers.len(), 1, "{:?}", discovery.exclusions);
            let quote = discovery.offers[0]
                .candidate
                .price
                .as_ref()
                .unwrap_or_else(|| panic!("complete native price"));
            assert_eq!(
                quote
                    .estimated_total(3600)
                    .unwrap_or_else(|_| panic!("estimated total")),
                if purchasing == "spot" {
                    Decimal::new(50155, 6)
                } else {
                    Decimal::new(118, 3)
                }
            );
            assert_eq!(quote.disk.hourly_rate, Some(Decimal::new(27, 3)));
            assert_eq!(quote.public_address.hourly_rate, Some(Decimal::new(5, 3)));
        } else {
            assert_eq!(discovery.offers.len(), 1, "{:?}", discovery.exclusions);
            let quote = discovery.offers[0]
                .candidate
                .price
                .as_ref()
                .unwrap_or_else(|| panic!("native base disk price"));
            assert_eq!(quote.disk.hourly_rate, None);
            assert!(
                quote
                    .disk
                    .source
                    .contains("I/O transaction usage is unpriced")
            );
            assert!(quote.estimated_total(3600).is_err());
        }
    }
}
