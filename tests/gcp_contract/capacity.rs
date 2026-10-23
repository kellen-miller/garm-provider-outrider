use super::identity::backend_for_shape;
use super::identity::{assert_quote_rejected, backend, query};
use std::{collections::BTreeSet, time::Duration};

use super::cloud_server::{MockRoutes, MockShape, start_server, start_server_with_routes};
use outrider::{cloud::CloudCatalog, policy::ProviderLaunchFacts};
use rust_decimal::Decimal;
#[tokio::test]
async fn bundled_local_ssd_uses_api_partition_count_and_attested_capacity() {
    let server = start_server_with_routes(MockRoutes {
        shape: MockShape::BundledSsd,
        advice_status: 200,
        advice_body: r#"{"recommendations":[]}"#.to_owned(),
        instances_status: 200,
        instances_body: r#"{"items":[]}"#.to_owned(),
    })
    .await
    .unwrap_or_else(|_| panic!("Local SSD contract server"));
    let backend = backend_for_shape(&server, false, MockShape::BundledSsd).await;
    let mut request = query("spot");
    request.profile.local_scratch_gib = 100;
    request
        .profile
        .machine_types
        .insert("gcp".to_owned(), vec!["n2-local-ssd-synthetic".to_owned()]);
    request
        .profile
        .administrator_limits
        .allowed_machine_types
        .insert(
            "gcp".to_owned(),
            BTreeSet::from(["n2-local-ssd-synthetic".to_owned()]),
        );
    let offers = backend
        .catalog(
            request,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("bundled SSD catalog"))
        .offers;
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].candidate.local_scratch_gib, Decimal::from(375u32));
    match &offers[0].launch_facts {
        ProviderLaunchFacts::Gcp(facts) => assert_eq!(facts.bundled_local_ssd_partitions, 1),
        _ => panic!("GCP launch facts"),
    }
    let quote = backend
        .quote(
            &offers[0],
            Duration::from_secs(3600),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|error| panic!("bundled SSD quote: {error}"));
    assert_eq!(
        quote.disk.hourly_rate,
        Some(
            Decimal::from(375u32) * Decimal::new(1, 3) + Decimal::new(4, 3) / Decimal::from(730u32)
        )
    );
    assert!(quote.disk.source.contains("LOCAL-SSD-SYNTHETIC"));
}

#[tokio::test]
async fn pre_attached_gpu_spot() {
    let server = start_server_with_routes(MockRoutes {
        shape: MockShape::GpuSpot,
        advice_status: 200,
        advice_body: r#"{"recommendations":[]}"#.to_owned(),
        instances_status: 200,
        instances_body: r#"{"items":[]}"#.to_owned(),
    })
    .await
    .unwrap_or_else(|_| panic!("GPU contract server"));
    let backend = backend_for_shape(&server, false, MockShape::GpuSpot).await;
    let mut gpu = query("spot");
    gpu.profile.image = "gpu-linux-synthetic".to_owned();
    gpu.profile.capabilities.insert("gpu-driver".to_owned());
    gpu.profile.administrator_limits.allowed_images =
        BTreeSet::from(["gpu-linux-synthetic".to_owned()]);
    gpu.profile
        .administrator_limits
        .allowed_machine_types
        .insert(
            "gcp".to_owned(),
            BTreeSet::from(["a2-highgpu-1g".to_owned()]),
        );
    gpu.profile.vcpu_max = 16;
    gpu.profile.memory_gib_max = 128;
    gpu.profile.gpu = Some(outrider::config::GpuRequirement {
        vendor: "nvidia".to_owned(),
        model: Some("A100".to_owned()),
        count: 1,
        memory_gib: Some(40),
    });
    gpu.profile.administrator_limits.max_gpu_count = 1;
    gpu.profile.administrator_limits.max_gpu_memory_gib = 80;
    gpu.profile.administrator_limits.allowed_gpu_vendors = BTreeSet::from(["nvidia".to_owned()]);
    gpu.profile.administrator_limits.allowed_gpu_models = BTreeSet::from(["A100".to_owned()]);
    gpu.profile.administrator_limits.max_vcpu = 16;
    gpu.profile.administrator_limits.max_memory_gib = 128;
    let offers = backend
        .catalog(gpu, tokio::time::Instant::now() + Duration::from_secs(5))
        .await
        .unwrap_or_else(|error| panic!("GPU catalog: {error}"))
        .offers;
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].candidate.gpu_model.as_deref(), Some("A100"));
    assert_eq!(
        offers[0].candidate.gpu_memory_gib,
        Some(Decimal::from(40u32))
    );
    assert!(
        !offers[0]
            .selection
            .approved_extras
            .contains_key("guestAccelerators")
    );
    let quote = backend
        .quote(
            &offers[0],
            Duration::from_secs(3600),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|error| panic!("GPU Spot quote: {error}"));
    assert_eq!(quote.compute.hourly_rate, Some(Decimal::new(1045, 3)));
    let mut tampered_market = offers[0].clone();
    tampered_market.candidate.purchasing = "on_demand".to_owned();
    tampered_market
        .selection
        .approved_extras
        .insert("spot".to_owned(), serde_json::json!(false));
    assert_quote_rejected(&backend, &tampered_market).await;

    let mut on_demand = query("on_demand");
    on_demand.profile.image = "gpu-linux-synthetic".to_owned();
    on_demand
        .profile
        .capabilities
        .insert("gpu-driver".to_owned());
    on_demand.profile.administrator_limits.allowed_images =
        BTreeSet::from(["gpu-linux-synthetic".to_owned()]);
    on_demand
        .profile
        .administrator_limits
        .allowed_machine_types
        .insert(
            "gcp".to_owned(),
            BTreeSet::from(["a2-highgpu-1g".to_owned()]),
        );
    on_demand.profile.vcpu_max = 16;
    on_demand.profile.memory_gib_max = 128;
    on_demand.profile.gpu = Some(outrider::config::GpuRequirement {
        vendor: "nvidia".to_owned(),
        model: Some("A100".to_owned()),
        count: 1,
        memory_gib: Some(40),
    });
    on_demand.profile.administrator_limits.max_gpu_count = 1;
    on_demand.profile.administrator_limits.max_gpu_memory_gib = 80;
    on_demand.profile.administrator_limits.allowed_gpu_vendors =
        BTreeSet::from(["nvidia".to_owned()]);
    on_demand.profile.administrator_limits.allowed_gpu_models = BTreeSet::from(["A100".to_owned()]);
    on_demand.profile.administrator_limits.max_vcpu = 16;
    on_demand.profile.administrator_limits.max_memory_gib = 128;
    assert!(
        backend
            .catalog(
                on_demand,
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .unwrap_or_default()
            .offers
            .is_empty()
    );
}

#[tokio::test]
async fn optional_capacity_advice_scope() {
    let server = start_server()
        .await
        .unwrap_or_else(|_| panic!("mock server"));
    let gcp_backend = backend(&server, true).await;
    let offers = gcp_backend
        .catalog(
            query("spot"),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("catalog"))
        .offers;
    let hint = offers[0]
        .candidate
        .native_capacity_hint
        .as_ref()
        .unwrap_or_else(|| panic!("scoped advice hint"));
    assert_eq!(hint.cloud, "gcp");
    assert_eq!(hint.account, "synthetic-project");
    assert_eq!(hint.location, "us-central1");
    assert_eq!(hint.score, Decimal::ZERO);
    let advice_body = {
        let requests = server
            .requests
            .lock()
            .unwrap_or_else(|_| panic!("request lock"));
        let advice = requests
            .iter()
            .find(|request| request.path.contains("/advice/capacity"))
            .unwrap_or_else(|| panic!("advice request"));
        assert_eq!(advice.method, "POST");
        advice.body.clone()
    };
    let body: serde_json::Value =
        serde_json::from_str(&advice_body).unwrap_or_else(|_| panic!("advice body"));
    assert_eq!(body["size"], 1);
    assert_eq!(
        body["distributionPolicy"]["zones"][0]["zone"],
        "zones/us-central1-a"
    );
    assert_eq!(
        body["instanceProperties"]["scheduling"]["provisioningModel"],
        "SPOT"
    );

    for advice in [
        MockRoutes {
            shape: MockShape::Standard,
            advice_status: 200,
            advice_body: r#"{"recommendations":[{"scores":{"obtainability":0.8},"shards":[{"zone":"us-central1-b","machineType":"n2-standard-4","instanceCount":1,"provisioningModel":"SPOT"}]}]}"#.to_owned(),
            instances_status: 200,
            instances_body: r#"{"items":[]}"#.to_owned(),
        },
        MockRoutes {
            shape: MockShape::Standard,
            advice_status: 503,
            advice_body: r#"{"error":"unavailable"}"#.to_owned(),
            instances_status: 200,
            instances_body: r#"{"items":[]}"#.to_owned(),
        },
    ] {
        let negative_server = start_server_with_routes(advice).await.unwrap_or_else(|_| panic!("negative advice server"));
        let negative_backend = backend(&negative_server, true).await;
        let negative_offers = negative_backend
            .catalog(query("spot"), tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .unwrap_or_else(|_| panic!("catalog with unavailable advice")).offers;
        assert_eq!(negative_offers.len(), 1);
        assert!(negative_offers[0].candidate.native_capacity_hint.is_none());
    }
}
