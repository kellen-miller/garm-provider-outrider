use std::{collections::BTreeSet, time::Duration};

use outrider::{
    cloud::CloudCatalog,
    config::{AzureImageLicensing, ConfigPatch},
};
use rust_decimal::Decimal;

use super::cloud_server::{
    MockResponse, backend, backend_with_storage_and_licensing, image_response, image_response_for,
    profile, query, retail_response, sku, start_server,
};

#[tokio::test]
async fn image_licensing_and_purchase_plan() {
    let free_server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
        },
        retail_response(),
        image_response(),
    ])
    .await
    .unwrap_or_else(|_| panic!("free image mock server"));
    let free_backend = backend(&free_server).await;
    let mut linux_profile = profile();
    linux_profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    let linux_offers = free_backend
        .catalog(
            query(linux_profile),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("attested free Linux image is eligible"))
        .offers;
    let linux_offer = linux_offers
        .first()
        .unwrap_or_else(|| panic!("Linux offer"));
    assert_eq!(linux_offer.selection.approved_extras.len(), 5);
    assert!(
        !linux_offer
            .selection
            .approved_extras
            .contains_key("image_licensing")
    );
    assert!(
        !linux_offer
            .selection
            .approved_extras
            .contains_key("image_licensing_source")
    );
    let outrider::policy::ProviderLaunchFacts::Azure(launch_facts) = &linux_offer.launch_facts
    else {
        panic!("Azure launch facts carry image licensing");
    };
    assert_eq!(
        launch_facts.image_licensing,
        AzureImageLicensing::NoAdditionalSoftwareCharge
    );
    assert_eq!(
        launch_facts.image_licensing_source,
        "operator verified Canonical image pricing documentation"
    );
    let linux_quote = free_backend
        .quote(
            linux_offer,
            Duration::from_secs(3600),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("Linux quote revalidates licensing"));
    assert_eq!(linux_quote.licensing.hourly_rate, Some(Decimal::ZERO));
    assert!(!linux_quote.licensing.included_in_compute);
    assert!(
        linux_quote
            .licensing
            .source
            .contains("No additional image software charge")
    );
    let mut tampered_offer = linux_offer.clone();
    let outrider::policy::ProviderLaunchFacts::Azure(tampered_facts) =
        &mut tampered_offer.launch_facts
    else {
        panic!("Azure launch facts");
    };
    tampered_facts.image_licensing_source.push_str(" changed");
    assert!(
        free_backend
            .quote(
                &tampered_offer,
                Duration::from_secs(3600),
                Duration::from_secs(600),
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    free_server
        .task
        .await
        .unwrap_or_else(|_| panic!("free server task"))
        .unwrap_or_else(|_| panic!("free server"));

    let windows_server = start_server(vec![
        image_response_for("Windows", None),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
        },
        retail_response(),
    ])
    .await
    .unwrap_or_else(|_| panic!("Windows image mock server"));
    let windows_backend = backend_with_storage_and_licensing(
        &windows_server,
        "Standard_LRS",
        AzureImageLicensing::WindowsLicenseInComputeMeter,
        "operator verified Azure Windows compute meter documentation",
    )
    .await;
    let mut windows_profile = profile();
    windows_profile.os = "windows".to_owned();
    windows_profile
        .administrator_limits
        .allowed_os
        .insert("windows".to_owned());
    windows_profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    let windows_offers = windows_backend
        .catalog(
            query(windows_profile),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("included Windows image is eligible"))
        .offers;
    let windows_offer = windows_offers
        .first()
        .unwrap_or_else(|| panic!("Windows offer"));
    let windows_quote = windows_offer
        .candidate
        .price
        .as_ref()
        .unwrap_or_else(|| panic!("Windows price"));
    assert_eq!(windows_quote.compute.hourly_rate, Some(Decimal::new(15, 2)));
    assert!(windows_quote.licensing.included_in_compute);
    assert!(
        windows_quote
            .licensing
            .source
            .contains("Windows license included in the compute meter")
    );
    windows_server
        .task
        .await
        .unwrap_or_else(|_| panic!("Windows server task"))
        .unwrap_or_else(|_| panic!("Windows server"));

    let unpriced_server = start_server(vec![image_response()])
        .await
        .unwrap_or_else(|_| panic!("unpriced image mock server"));
    let unpriced_backend = backend_with_storage_and_licensing(
        &unpriced_server,
        "Standard_LRS",
        AzureImageLicensing::AdditionalUnpricedSoftware,
        "operator verified marketplace software pricing documentation",
    )
    .await;
    assert!(
        unpriced_backend
            .catalog(
                query(profile()),
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    unpriced_server
        .task
        .await
        .unwrap_or_else(|_| panic!("unpriced server task"))
        .unwrap_or_else(|_| panic!("unpriced server"));

    let plan = serde_json::json!({"name":"example","publisher":"example","product":"example"});
    let planned_server = start_server(vec![image_response_for("Linux", Some(plan.clone()))])
        .await
        .unwrap_or_else(|_| panic!("planned image mock server"));
    let planned_backend = backend(&planned_server).await;
    assert!(
        planned_backend
            .catalog(
                query(profile()),
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    {
        let requests = planned_server
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("planned requests lock"));
        assert_eq!(
            requests.len(),
            1,
            "plan rejected before SKU discovery and pricing"
        );
    }
    planned_server
        .task
        .await
        .unwrap_or_else(|_| panic!("planned server task"))
        .unwrap_or_else(|_| panic!("planned server"));

    let quote_plan_server = start_server(vec![
        image_response(),
        MockResponse {
            status: 200,
            body: serde_json::json!({"value":[sku("Standard_D2s_v3")]}).to_string(),
        },
        retail_response(),
        image_response_for("Linux", Some(plan)),
    ])
    .await
    .unwrap_or_else(|_| panic!("quote plan mock server"));
    let quote_plan_backend = backend(&quote_plan_server).await;
    let mut profile = profile();
    profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    let offers = quote_plan_backend
        .catalog(
            query(profile),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog before quote plan"))
        .offers;
    let quote_result = quote_plan_backend
        .quote(
            offers.first().unwrap_or_else(|| panic!("offer")),
            Duration::from_secs(3600),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await;
    assert!(
        quote_result.is_err(),
        "plan appearing at quote invalidates the offer"
    );
    quote_plan_server
        .task
        .await
        .unwrap_or_else(|_| panic!("quote plan server task"))
        .unwrap_or_else(|_| panic!("quote plan server"));

    let missing = ConfigPatch::from_yaml(
        "version: 1\nazure_catalog_bindings:\n  - id: a\n    provider_binding_id: p\n    account_id: a\n    subscription_id: s\n    pricing_location: eastus\n    credential_reference: provider-binding:p:configuration\n    locations: {}\n    images:\n      linux:\n        image_id: Canonical:Ubuntu:22:1\n        minimum_os_disk_gib: 1\n        minimum_os_disk_source: source\n        capabilities: []\n    storage_classes: {}\n    accelerator_facts: {}\n",
    );
    assert!(
        missing.is_err(),
        "licensing and provenance are required centrally"
    );
    let unknown = serde_json::from_str::<AzureImageLicensing>("\"unknown\"");
    assert!(unknown.is_err(), "unknown licensing states fail closed");
}
