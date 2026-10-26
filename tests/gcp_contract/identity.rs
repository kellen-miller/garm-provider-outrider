use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

use google_cloud_auth::credentials::AccessTokenCredentials;
use outrider::{
    cloud::{
        CatalogQuery, CloudCatalog, PlacementIdentity,
        gcp::{GcpBackend, GcpImage, GcpSettings},
    },
    config::{
        GcpAcceleratorFacts, GcpDiskPerformance, GcpLicenseBasis, GcpLicenseFacts, GcpMachineFacts,
    },
    policy::CatalogOffer,
};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};

use super::cloud_server::{
    CHILD_CONFIG, MockRoutes, MockServer, MockShape, StaticCredential, start_server,
    start_server_with_routes,
};

#[tokio::test]
async fn gcp_preemption_identity_time_error_and_page_failures_stay_unknown() {
    let valid: serde_json::Value = serde_json::from_str(r#"{"items":[{"id":"15041793718812375372","name":"systemevent-1","operationType":"compute.instances.preempted","targetLink":"https://www.googleapis.com/compute/v1/projects/synthetic-project/zones/us-central1-a/instances/runner-native","targetId":"12820389800990687211","status":"DONE","startTime":"2026-10-21T16:00:00Z"}]}"#)
        .unwrap_or_else(|_| panic!("valid operation fixture"));
    let mut wrong_project = valid.clone();
    wrong_project["items"][0]["targetLink"] = serde_json::json!(
        "https://www.googleapis.com/compute/v1/projects/foreign/zones/us-central1-a/instances/runner-native"
    );
    let mut missing_time = valid.clone();
    missing_time["items"][0]
        .as_object_mut()
        .unwrap_or_else(|| panic!("operation object"))
        .remove("startTime");
    let mut operation_error = valid.clone();
    operation_error["items"][0]["error"] =
        serde_json::json!({"errors":[{"code":"INTERNAL_ERROR"}]});
    let mut not_done = valid.clone();
    not_done["items"][0]["status"] = serde_json::json!("RUNNING");
    let identity = PlacementIdentity {
        controller_id: "controller-1".to_owned(),
        cloud: "gcp".to_owned(),
        account_id: "synthetic-project".to_owned(),
        location: "us-central1-a".to_owned(),
        resource_id: "12820389800990687211".to_owned(),
        host_id: None,
        dependency_ids: BTreeMap::from([("instance_name".to_owned(), "runner-native".to_owned())]),
    };
    for body in [wrong_project, operation_error, not_done] {
        let server = start_server_with_routes(MockRoutes {
            shape: MockShape::Standard,
            advice_status: 200,
            advice_body: body.to_string(),
            instances_status: 200,
            instances_body: r#"{"items":[]}"#.to_owned(),
        })
        .await
        .unwrap_or_else(|_| panic!("negative operation fixture server"));
        let backend = backend(&server, false).await;
        assert!(
            backend
                .observe_interruption(
                    &identity,
                    tokio::time::Instant::now() + Duration::from_secs(5)
                )
                .await
                .unwrap_or_else(|_| panic!("negative operation lookup"))
                .is_empty()
        );
    }
    let server = start_server_with_routes(MockRoutes {
        shape: MockShape::Standard,
        advice_status: 200,
        advice_body: missing_time.to_string(),
        instances_status: 200,
        instances_body: r#"{"items":[]}"#.to_owned(),
    })
    .await
    .unwrap_or_else(|_| panic!("missing-time operation fixture server"));
    let missing_time_backend = backend(&server, false).await;
    assert!(
        missing_time_backend
            .observe_interruption(
                &identity,
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .expect_err("completed preemption without provider time is unknown")
            .to_string()
            .contains("no provider time")
    );

    let mut conflicting_revision = valid.clone();
    let conflicting = conflicting_revision["items"][0].clone();
    conflicting_revision["items"][0]["status"] = serde_json::json!("RUNNING");
    conflicting_revision["items"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("operation rows"))
        .push(conflicting);
    let server = start_server_with_routes(MockRoutes {
        shape: MockShape::Standard,
        advice_status: 200,
        advice_body: conflicting_revision.to_string(),
        instances_status: 200,
        instances_body: r#"{"items":[]}"#.to_owned(),
    })
    .await
    .unwrap_or_else(|_| panic!("conflicting operation fixture server"));
    let conflict_backend = backend(&server, false).await;
    let error = conflict_backend
        .observe_interruption(
            &identity,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect_err("conflicting same-operation revisions are unknown");
    assert!(error.to_string().contains("conflicting facts"));

    let server = start_server_with_routes(MockRoutes {
        shape: MockShape::Standard,
        advice_status: 200,
        advice_body: "fail-second-operation-page".to_owned(),
        instances_status: 200,
        instances_body: r#"{"items":[]}"#.to_owned(),
    })
    .await
    .unwrap_or_else(|_| panic!("operation page fixture server"));
    let backend = backend(&server, false).await;
    let error = backend
        .observe_interruption(
            &identity,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect_err("incomplete operation pagination is unknown");
    assert!(error.to_string().contains("unsuccessful status"));
}

#[tokio::test]
async fn spot_preemption_observation_matches_unsigned_native_identity() {
    let server = start_server_with_routes(MockRoutes {
        shape: MockShape::Standard,
        advice_status: 200,
        advice_body: r#"{"items":[{"id":"15041793718812375372","name":"systemevent-1","operationType":"compute.instances.preempted","targetLink":"https://www.googleapis.com/compute/v1/projects/synthetic-project/zones/us-central1-a/instances/runner-native","targetId":"12820389800990687211","status":"DONE","startTime":"2026-10-21T16:00:00Z","endTime":"2026-10-21T16:00:00Z"}]}"#.to_owned(),
        instances_status: 200,
        instances_body: r#"{"items":[]}"#.to_owned(),
    })
    .await
    .unwrap_or_else(|_| panic!("operation fixture server"));
    let backend = backend(&server, false).await;
    let identity = PlacementIdentity {
        controller_id: "controller-1".to_owned(),
        cloud: "gcp".to_owned(),
        account_id: "synthetic-project".to_owned(),
        location: "us-central1-a".to_owned(),
        resource_id: "12820389800990687211".to_owned(),
        host_id: None,
        dependency_ids: BTreeMap::from([("instance_name".to_owned(), "runner-native".to_owned())]),
    };
    let evidence = backend
        .observe_interruption(
            &identity,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("preemption operation lookup"));
    let evidence = evidence
        .first()
        .unwrap_or_else(|| panic!("verified preemption"));
    assert_eq!(evidence.event_id, "systemevent-1");
    assert_eq!(evidence.occurred_at, "2026-10-21T16:00:00Z");
    let mut foreign_generation = identity;
    foreign_generation.resource_id = "12820389800990687212".to_owned();
    assert!(
        backend
            .observe_interruption(
                &foreign_generation,
                tokio::time::Instant::now() + Duration::from_secs(5)
            )
            .await
            .unwrap_or_else(|_| panic!("foreign generation lookup"))
            .is_empty()
    );
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request capture"));
    assert!(
        requests
            .iter()
            .all(|request| request.authorization.as_deref() == Some("Bearer contract-token"))
    );
    assert!(requests.iter().all(|request| {
        request
            .path
            .contains("/projects/synthetic-project/zones/us-central1-a/operations")
    }));
}

#[tokio::test]
async fn compute_pagination_rejects_repeated_next_page_token() {
    let server = start_server_with_routes(MockRoutes {
        shape: MockShape::Standard,
        advice_status: 200,
        advice_body: "repeat-compute-page-token".to_owned(),
        instances_status: 200,
        instances_body: r#"{"items":[]}"#.to_owned(),
    })
    .await
    .unwrap_or_else(|_| panic!("repeated Compute page-token server"));
    let backend = backend(&server, false).await;
    let error = backend
        .catalog(
            query("spot"),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect_err("repeated Compute page token must fail");
    assert!(error.to_string().contains("pagination"));
    let machine_requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("request lock"))
        .iter()
        .filter(|request| request.path.contains("/machineTypes"))
        .count();
    assert_eq!(machine_requests, 2);
}

#[tokio::test]
async fn catalog_reports_image_architecture_exclusion() {
    let server = start_server()
        .await
        .unwrap_or_else(|_| panic!("mock server"));
    let backend = backend(&server, false).await;
    let mut request = query("spot");
    request.profile.architecture = "arm64".to_owned();
    let discovery = backend
        .catalog(
            request,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|_| panic!("GCP catalog"));
    assert!(discovery.queried);
    assert!(discovery.offers.is_empty());
    assert!(discovery.exclusions.iter().any(|exclusion| {
        exclusion.reason
            == "machine or image does not meet requested platform, compute, family, capability, or GPU bounds"
            && exclusion.observed_count == 1
    }));
    server.task.abort();
}

pub(super) fn machine_page(shape: MockShape, paginated: bool) -> String {
    if paginated {
        r#"{"items":[],"nextPageToken":"second/page"}"#.to_owned()
    } else {
        format!("{{\"items\":[{}]}}", machine_json(shape))
    }
}

pub(super) fn machine_json(shape: MockShape) -> String {
    match shape {
        MockShape::Standard => r#"{"name":"n2-standard-4","guestCpus":4,"memoryMb":16384,"architecture":"X86_64","isSharedCpu":false,"accelerators":[]}"#.to_owned(),
        MockShape::NativeSpot => r#"{"name":"e2-standard-8","guestCpus":8,"memoryMb":32768,"isSharedCpu":false,"accelerators":[]}"#.to_owned(),
        MockShape::WindowsSql => r#"{"name":"n2-standard-2","guestCpus":2,"memoryMb":8192,"architecture":"X86_64","isSharedCpu":false,"accelerators":[]}"#.to_owned(),
        MockShape::GpuSpot => r#"{"name":"a2-highgpu-1g","guestCpus":12,"memoryMb":87040,"architecture":"X86_64","isSharedCpu":false,"accelerators":[{"guestAcceleratorType":"nvidia-tesla-a100","guestAcceleratorCount":1}]}"#.to_owned(),
        MockShape::BundledSsd => r#"{"name":"n2-local-ssd-synthetic","guestCpus":4,"memoryMb":16384,"architecture":"X86_64","isSharedCpu":false,"accelerators":[],"bundledLocalSsds":{"partitionCount":1,"defaultInterface":"NVME"}}"#.to_owned(),
    }
}

pub(super) fn image_json(shape: MockShape) -> String {
    let (name, licenses) = match shape {
        MockShape::Standard | MockShape::NativeSpot => ("ubuntu-24", Vec::new()),
        MockShape::WindowsSql => (
            "windows-server-synthetic",
            vec![
                "projects/synthetic-windows/global/licenses/windows-server-synthetic",
                "projects/synthetic-sql/global/licenses/sql-server-standard-synthetic",
            ],
        ),
        MockShape::GpuSpot => ("linux-gpu-driver-synthetic", Vec::new()),
        MockShape::BundledSsd => ("ubuntu-24", Vec::new()),
    };
    serde_json::json!({
        "selfLink": format!("https://www.googleapis.com/compute/v1/projects/synthetic-project/global/images/{name}"),
        "architecture": "X86_64",
        "diskSizeGb": "20",
        "status": "READY",
        "licenses": licenses,
    }).to_string()
}

pub(super) fn windows_skus(second_page: bool) -> String {
    if second_page {
        r#"{"skus":[{"name":"services/6F81-5844-456A/skus/RAM-N2-WINDOWS","description":"N2 Instance RAM","category":{"resourceFamily":"Compute","resourceGroup":"RAM","usageType":"Preemptible"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"GiBy.h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":5000000}}]}}]},{"name":"services/6F81-5844-456A/skus/PD-BALANCED","description":"Balanced PD","category":{"resourceFamily":"Storage","resourceGroup":"PD","usageType":"OnDemand"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"GiBy.mo","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":100000}}]}}]},{"name":"services/6F81-5844-456A/skus/WINDOWS-SYNTHETIC","description":"Windows Server license","category":{"resourceFamily":"License","resourceGroup":"Windows","usageType":"OnDemand"},"serviceRegions":[],"geoTaxonomy":{"type":"GLOBAL"},"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":20000000}}]}}]},{"name":"services/6F81-5844-456A/skus/SQL-SYNTHETIC","description":"SQL Server license","category":{"resourceFamily":"License","resourceGroup":"SQL","usageType":"OnDemand"},"serviceRegions":[],"geoTaxonomy":{"type":"GLOBAL"},"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":30000000}}]}}]}]}"#.to_owned()
    } else {
        r#"{"skus":[{"name":"services/6F81-5844-456A/skus/CPU-N2-WINDOWS","description":"N2 Instance Core","category":{"resourceFamily":"Compute","resourceGroup":"CPU","usageType":"Preemptible"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":10000000}}]}}]}],"nextPageToken":"pricing/page"}"#.to_owned()
    }
}

pub(super) fn gpu_skus(second_page: bool) -> String {
    if second_page {
        r#"{"skus":[{"name":"services/6F81-5844-456A/skus/RAM-A2-SPOT","description":"A2 Instance RAM","category":{"resourceFamily":"Compute","resourceGroup":"RAM","usageType":"Preemptible"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"GiBy.h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":5000000}}]}}]},{"name":"services/6F81-5844-456A/skus/GPU-A100-SPOT","description":"NVIDIA A100 GPU","category":{"resourceFamily":"Compute","resourceGroup":"GPU","usageType":"Preemptible"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":500000000}}]}}]},{"name":"services/6F81-5844-456A/skus/PD-BALANCED","description":"Balanced PD","category":{"resourceFamily":"Storage","resourceGroup":"PD","usageType":"OnDemand"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"GiBy.mo","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":100000}}]}}]}]}"#.to_owned()
    } else {
        r#"{"skus":[{"name":"services/6F81-5844-456A/skus/CPU-A2-SPOT","description":"A2 Instance Core","category":{"resourceFamily":"Compute","resourceGroup":"CPU","usageType":"Preemptible"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":10000000}}]}}]}],"nextPageToken":"pricing/page"}"#.to_owned()
    }
}

fn child_path() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "outrider-gcp-contract-{}-{}.toml",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&path, CHILD_CONFIG).unwrap_or_else(|_| panic!("write child config"));
    path
}

fn image(shape: MockShape) -> GcpImage {
    let (name, os, capabilities, license_free_source, license_facts) = match shape {
        MockShape::Standard | MockShape::NativeSpot => (
            "ubuntu-24",
            "linux",
            BTreeSet::from(["docker".to_owned()]),
            Some("operator verified image license state".to_owned()),
            BTreeMap::new(),
        ),
        MockShape::WindowsSql => (
            "windows-server-synthetic",
            "windows",
            BTreeSet::from(["docker".to_owned()]),
            None,
            BTreeMap::from([
                (
                    "projects/synthetic-windows/global/licenses/windows-server-synthetic"
                        .to_owned(),
                    GcpLicenseFacts {
                        billing_sku: Some(
                            "services/6F81-5844-456A/skus/WINDOWS-SYNTHETIC".to_owned(),
                        ),
                        basis: GcpLicenseBasis::PerVcpu,
                        source: "synthetic contract mapping of visible-vCPU Windows billing"
                            .to_owned(),
                    },
                ),
                (
                    "projects/synthetic-sql/global/licenses/sql-server-standard-synthetic"
                        .to_owned(),
                    GcpLicenseFacts {
                        billing_sku: Some("services/6F81-5844-456A/skus/SQL-SYNTHETIC".to_owned()),
                        basis: GcpLicenseBasis::SqlVcpuFloor,
                        source: "synthetic contract mapping of SQL max(vCPU,4) billing".to_owned(),
                    },
                ),
            ]),
        ),
        MockShape::GpuSpot => (
            "linux-gpu-driver-synthetic",
            "linux",
            BTreeSet::from(["docker".to_owned(), "gpu-driver".to_owned()]),
            Some("synthetic exact image driver capability attestation".to_owned()),
            BTreeMap::new(),
        ),
        MockShape::BundledSsd => (
            "ubuntu-24",
            "linux",
            BTreeSet::from(["docker".to_owned()]),
            Some("synthetic operator verified free image".to_owned()),
            BTreeMap::new(),
        ),
    };
    GcpImage {
        image_id: format!("projects/synthetic-project/global/images/{name}"),
        os: os.to_owned(),
        os_source: format!("synthetic source-attested {os} image mapping"),
        architecture: "amd64".to_owned(),
        capabilities,
        capabilities_source: "synthetic source-attested image capabilities".to_owned(),
        license_free_source,
        license_facts,
    }
}

pub(super) fn settings(endpoint: &str, enabled_advice: bool, shape: MockShape) -> GcpSettings {
    let config_path = child_path();
    let bytes = std::fs::read(&config_path).unwrap_or_else(|_| panic!("read child config"));
    GcpSettings {
        catalog_id: "gcp-zonal".to_owned(),
        project_id: "synthetic-project".to_owned(),
        zone: "us-central1-a".to_owned(),
        pricing_location: "us-central1".to_owned(),
        images: BTreeMap::from([(
            match shape {
                MockShape::Standard => "ubuntu-24",
                MockShape::NativeSpot => "ubuntu-24",
                MockShape::WindowsSql => "windows-synthetic",
                MockShape::GpuSpot => "gpu-linux-synthetic",
                MockShape::BundledSsd => "ubuntu-24",
            }
            .to_owned(),
            image(shape),
        )]),
        storage_classes: BTreeMap::from([("balanced".to_owned(), "pd-balanced".to_owned())]),
        storage_billing_skus: BTreeMap::from([(
            "balanced".to_owned(),
            "services/6F81-5844-456A/skus/PD-BALANCED".to_owned(),
        )]),
        external_ip_billing_sku: None,
        accelerator_facts: if matches!(shape, MockShape::GpuSpot) {
            BTreeMap::from([(
                "nvidia-tesla-a100".to_owned(),
                GcpAcceleratorFacts {
                    vendor: "nvidia".to_owned(),
                    model: "A100".to_owned(),
                    memory_gib: Decimal::from(40u32),
                    spot_billing_sku: "services/6F81-5844-456A/skus/GPU-A100-SPOT".to_owned(),
                    source: "synthetic source-attested A100 facts".to_owned(),
                },
            )])
        } else {
            BTreeMap::<String, GcpAcceleratorFacts>::new()
        },
        machine_facts: BTreeMap::from([(
            match shape {
                MockShape::Standard => "n2-standard-4",
                MockShape::NativeSpot => "e2-standard-8",
                MockShape::WindowsSql => "n2-standard-2",
                MockShape::GpuSpot => "a2-highgpu-1g",
                MockShape::BundledSsd => "n2-local-ssd-synthetic",
            }
            .to_owned(),
            GcpMachineFacts {
                architecture: matches!(shape, MockShape::NativeSpot).then(|| "amd64".to_owned()),
                shared_cpu_billing_sku: None,
                bundled_local_ssd_gib: matches!(shape, MockShape::BundledSsd).then_some(375),
                bundled_local_ssd_partitions: matches!(shape, MockShape::BundledSsd).then_some(1),
                local_ssd_billing_sku: matches!(shape, MockShape::BundledSsd)
                    .then(|| "services/6F81-5844-456A/skus/LOCAL-SSD-SYNTHETIC".to_owned()),
                local_ssd_billing_quantity: matches!(shape, MockShape::BundledSsd)
                    .then_some(Decimal::from(375u32)),
                max_pd_iops: Some(10000),
                max_pd_throughput_mib_s: Some(Decimal::from(500u32)),
                source: "operator verified exact machine limits".to_owned(),
            },
        )]),
        disk_performance: BTreeMap::from([(
            "balanced".to_owned(),
            GcpDiskPerformance {
                base_iops: 3000,
                iops_per_gib: 6,
                maximum_iops: 80000,
                base_throughput_mib_s: Decimal::from(140u32),
                throughput_per_gib_mib_s: Decimal::new(3, 1),
                maximum_throughput_mib_s: Decimal::from(1200u32),
                source: "operator verified persistent disk performance".to_owned(),
            },
        )]),
        capacity_advice_enabled: enabled_advice,
        binding_id: "gcp-child".to_owned(),
        source_revision: "98a362f5319928099b942a668c5a17a34170066a".to_owned(),
        binary_sha256: "a".repeat(64),
        configuration_sha256: hex::encode(Sha256::digest(bytes)),
        purchasing_capabilities: BTreeSet::from(["spot".to_owned(), "on_demand".to_owned()]),
        provider_configuration_path: config_path,
        compute_endpoint: endpoint.to_owned(),
        billing_endpoint: endpoint.to_owned(),
    }
}

#[tokio::test]
async fn missing_machine_architecture_is_not_assumed_from_the_image() {
    let server = start_server_with_routes(MockRoutes {
        shape: MockShape::NativeSpot,
        advice_status: 200,
        advice_body: "{}".to_owned(),
        instances_status: 200,
        instances_body: r#"{"items":[]}"#.to_owned(),
    })
    .await
    .unwrap();
    let mut configuration = settings(&server.endpoint, false, MockShape::NativeSpot);
    configuration
        .machine_facts
        .get_mut("e2-standard-8")
        .unwrap()
        .architecture = None;
    let backend = GcpBackend::with_clients(
        configuration,
        reqwest::Client::new(),
        AccessTokenCredentials::from(StaticCredential),
        tokio::time::Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    let mut request = query("spot");
    request
        .profile
        .administrator_limits
        .allowed_machine_types
        .insert(
            "gcp".to_owned(),
            BTreeSet::from(["e2-standard-8".to_owned()]),
        );
    let discovery = backend
        .catalog(
            request,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(discovery.offers.is_empty());
    assert!(discovery.exclusions.iter().any(|exclusion| {
        exclusion
            .reason
            .contains("lacks native or attested architecture")
    }));
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| !request.path.contains("/images/"))
    );
}

#[tokio::test]
async fn attested_architecture_cannot_override_native_facts_or_requotes() {
    let server = start_server().await.unwrap();
    let original = backend(&server, false).await;
    let offers = original
        .catalog(
            query("spot"),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap()
        .offers;
    let mut configuration = settings(&server.endpoint, false, MockShape::Standard);
    configuration
        .machine_facts
        .get_mut("n2-standard-4")
        .unwrap()
        .architecture = Some("arm64".to_owned());
    let conflicting = GcpBackend::with_clients(
        configuration,
        reqwest::Client::new(),
        AccessTokenCredentials::from(StaticCredential),
        tokio::time::Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    let error = conflicting
        .catalog(
            query("spot"),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("architectures conflict"));
    let error = conflicting
        .quote(
            &offers[0],
            Duration::from_secs(60),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("architectures conflict"));
}

pub(super) async fn backend(server: &MockServer, enabled_advice: bool) -> GcpBackend {
    backend_for_shape(server, enabled_advice, MockShape::Standard).await
}

pub(super) async fn backend_for_shape(
    server: &MockServer,
    enabled_advice: bool,
    shape: MockShape,
) -> GcpBackend {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| panic!("HTTP client"));
    GcpBackend::with_clients(
        settings(&server.endpoint, enabled_advice, shape),
        client,
        AccessTokenCredentials::from(StaticCredential),
        tokio::time::Instant::now() + Duration::from_secs(10),
    )
    .await
    .unwrap_or_else(|_| panic!("valid GCP binding"))
}

pub(super) async fn assert_quote_rejected(backend: &GcpBackend, offer: &CatalogOffer) {
    assert!(
        backend
            .quote(
                offer,
                Duration::from_secs(60),
                Duration::from_secs(600),
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .is_err()
    );
}

pub(super) fn query(purchasing: &str) -> CatalogQuery {
    let patch =
        outrider::config::CentralConfig::from_toml(include_str!("../../examples/central.toml"))
            .and_then(|source| source.load_native_configuration(&std::collections::BTreeMap::new()))
            .unwrap_or_else(|_| panic!("central config"));
    let mut profile = patch
        .normalize_profile(None, "linux-large", None)
        .unwrap_or_else(|_| panic!("central profile"))
        .profile;
    profile.os = "linux".to_owned();
    profile.architecture = "amd64".to_owned();
    profile.image = "ubuntu-24".to_owned();
    profile.vcpu_min = 1;
    profile.vcpu_max = 8;
    profile.memory_gib_min = 1;
    profile.memory_gib_max = 32;
    profile.machine_types.remove("aws");
    profile.accounts = vec!["synthetic-project".to_owned()];
    profile.locations = vec!["us-central1".to_owned()];
    profile.boot_disk.class = "balanced".to_owned();
    profile.boot_disk.size_gib = 40;
    profile.boot_disk.iops = None;
    profile.boot_disk.throughput_mib_s = None;
    profile.local_scratch_gib = 0;
    profile.purchasing = purchasing.to_owned();
    profile.purchasing_models = BTreeSet::from([purchasing.to_owned()]);
    profile.administrator_limits.allowed_clouds = BTreeSet::from(["gcp".to_owned()]);
    profile.administrator_limits.allowed_accounts =
        BTreeSet::from(["synthetic-project".to_owned()]);
    profile.administrator_limits.allowed_locations = BTreeSet::from(["us-central1".to_owned()]);
    profile.administrator_limits.allowed_machine_types = BTreeMap::from([(
        "gcp".to_owned(),
        BTreeSet::from(["n2-standard-4".to_owned()]),
    )]);
    profile.administrator_limits.allowed_gpu_vendors.clear();
    profile.administrator_limits.allowed_gpu_models.clear();
    profile.administrator_limits.allowed_purchasing = BTreeSet::from([purchasing.to_owned()]);
    CatalogQuery {
        clouds: vec!["gcp".to_owned()],
        accounts: vec!["synthetic-project".to_owned()],
        locations: vec!["us-central1".to_owned()],
        profile,
    }
}
