use super::config::{candidate, charge, profile};
use std::collections::BTreeSet;
use std::io;
use std::sync::{Arc, Mutex};

use http_body_util::BodyExt;
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use outrider::policy::{
    Candidate, Policy, PolicyError, PriceComponent, PriceQuote, WeightedPolicy, rank,
};

use rust_decimal::Decimal;
use serde::Serialize;
use tokio::net::TcpListener;

struct CapturedRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

struct MockResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

struct ContractServer {
    endpoint: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    task: tokio::task::JoinHandle<io::Result<()>>,
}

async fn start_contract_server(responses: Vec<MockResponse>) -> io::Result<ContractServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let task = tokio::spawn(async move {
        for mock in responses {
            let (stream, _) = listener.accept().await?;
            serve_mock_response(stream, mock, recorded.clone()).await?;
        }

        Ok(())
    });
    Ok(ContractServer {
        endpoint: format!("http://{address}"),
        requests,
        task,
    })
}

async fn serve_mock_response(
    stream: tokio::net::TcpStream,
    mock: MockResponse,
    recorded: Arc<Mutex<Vec<CapturedRequest>>>,
) -> io::Result<()> {
    let service = service_fn(move |request: hyper::Request<Incoming>| {
        let recorded = recorded.clone();
        let status = mock.status;
        let headers = mock.headers.clone();
        let response_body = mock.body.clone();
        async move {
            let (parts, body) = request.into_parts();
            let request_body = body.collect().await.map_err(io::Error::other)?.to_bytes();
            let request = CapturedRequest {
                method: parts.method.to_string(),
                path: parts.uri.to_string(),
                authorization: parts
                    .headers
                    .get(hyper::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                body: String::from_utf8_lossy(&request_body).into_owned(),
            };

            recorded
                .lock()
                .map_err(|_| io::Error::other("contract request recording lock is poisoned"))?
                .push(request);
            let mut response = hyper::Response::builder()
                .status(status)
                .header("connection", "close");
            for (name, value) in headers {
                response = response.header(name, value);
            }

            response
                .body(http_body_util::Full::new(bytes::Bytes::from(response_body)))
                .map_err(io::Error::other)
        }
    });
    http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
        .map_err(io::Error::other)
}

fn mock(status: u16, content_type: &str, body: String) -> MockResponse {
    MockResponse {
        status,
        headers: vec![("content-type".to_owned(), content_type.to_owned())],
        body,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureVmRequest {
    location: String,
    properties: AzureVmProperties,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureVmProperties {
    hardware_profile: AzureHardwareProfile,
    storage_profile: AzureStorageProfile,
    network_profile: AzureNetworkProfile,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureHardwareProfile {
    vm_size: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureStorageProfile {
    image_reference: AzureImageReference,
    os_disk: AzureOsDisk,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureImageReference {
    publisher: String,
    offer: String,
    sku: String,
    version: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureOsDisk {
    create_option: String,
    managed_disk: AzureManagedDisk,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureManagedDisk {
    storage_account_type: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureNetworkProfile {
    network_interfaces: Vec<AzureNetworkInterface>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureNetworkInterface {
    id: String,
    properties: AzurePrimaryInterface,
}

#[derive(Serialize)]
struct AzurePrimaryInterface {
    primary: bool,
}

#[test]
fn latency_weight_uses_measured_latency_freshness_and_samples() {
    let mut candidate = candidate("latency-history", Decimal::ONE);
    candidate.mean_create_seconds = Some(Decimal::from(10));
    candidate.definitive_create_outcomes = 100;
    candidate.measured_latency_samples = 1;
    candidate.observation_samples = 100;
    candidate
        .observation_window
        .as_mut()
        .unwrap()
        .minimum_samples = 2;
    candidate.observation_ages.as_mut().unwrap().latency_seconds = Some(0);
    let policy = Policy::Weighted(WeightedPolicy {
        cost_weight: Decimal::ZERO,
        failure_weight: Decimal::ZERO,
        interruption_weight: Decimal::ZERO,
        latency_weight: Decimal::ONE,
        cost_scale: Decimal::ONE,
        interruption_scale: Decimal::ONE,
        latency_scale: Decimal::ONE,
        min_samples: 2,
        max_observation_age_seconds: 600,
        max_price_age_seconds: 600,
    });
    assert_eq!(
        rank(&policy, &[candidate.clone()], &profile().unwrap()),
        Err(PolicyError::MissingData("adequate recent observations"))
    );

    candidate.measured_latency_samples = 2;
    candidate.observation_ages.as_mut().unwrap().latency_seconds = Some(601);
    assert_eq!(
        rank(&policy, &[candidate.clone()], &profile().unwrap()),
        Err(PolicyError::MissingData("adequate recent observations"))
    );

    candidate.observation_ages.as_mut().unwrap().latency_seconds = Some(0);
    assert!(rank(&policy, &[candidate], &profile().unwrap()).is_ok());
}

#[test]
fn total_cost_commitment_overflow() {
    let quote = PriceQuote {
        provider_request_ids: Vec::new(),
        currency: "USD".to_owned(),
        compute: PriceComponent {
            minimum_commitment_seconds: 86_400,
            ..charge(Decimal::from(2))
        },
        disk: charge(Decimal::new(20, 2)),
        licensing: charge(Decimal::new(30, 2)),
        public_address: charge(Decimal::ZERO),
        new_host_minimum_commitment_seconds: Some(86_400),
        existing_host_remaining_commitment_seconds: None,
        observed_age_seconds: Some(60),
        included_charge_names: BTreeSet::new(),
        exclusions: Vec::new(),
    };

    assert_eq!(quote.estimated_total(3600).unwrap(), Decimal::new(485, 1));
    let existing_host = PriceQuote {
        existing_host_remaining_commitment_seconds: Some(3600),
        ..quote.clone()
    };

    assert_eq!(
        existing_host
            .estimated_existing_host_marginal_total(3600)
            .unwrap(),
        Decimal::new(5, 1)
    );
    assert_eq!(
        PriceQuote {
            provider_request_ids: Vec::new(),
            currency: "EUR".to_owned(),
            ..quote.clone()
        }
        .estimated_total(3600)
        .unwrap_err(),
        PolicyError::UnsupportedCurrency
    );
    assert_eq!(
        PriceQuote {
            compute: PriceComponent {
                hourly_rate: Some(Decimal::NEGATIVE_ONE),
                ..charge(Decimal::ZERO)
            },
            ..quote.clone()
        }
        .estimated_total(3600)
        .unwrap_err(),
        PolicyError::NegativePrice
    );
    let too_large = PriceQuote {
        compute: PriceComponent {
            hourly_rate: Some(Decimal::MAX),
            ..charge(Decimal::ZERO)
        },
        ..quote.clone()
    };

    assert_eq!(
        too_large.estimated_total(u64::MAX).unwrap_err(),
        PolicyError::CostOverflow
    );
    assert_eq!(
        existing_host.estimated_total(3600).unwrap(),
        Decimal::new(5, 1)
    );
    assert_commitment_cost_ranking(quote, existing_host).unwrap();
}

fn assert_commitment_cost_ranking(
    quote: PriceQuote,
    existing_host: PriceQuote,
) -> Result<(), Box<dyn std::error::Error>> {
    let existing_candidate = Candidate {
        price: Some(existing_host.clone()),
        ..candidate("existing-host", Decimal::from(2))
    };

    let new_candidate = Candidate {
        price: Some(quote),
        ..candidate("new-host", Decimal::from(2))
    };

    let mut no_ceiling = profile()?;
    no_ceiling.maximum_total_cost = None;
    no_ceiling.administrator_limits.max_total_cost = None;
    let economy = Policy::Economy {
        max_price_age_seconds: 600,
    };

    let ranked = rank(
        &economy,
        &[new_candidate.clone(), existing_candidate.clone()],
        &no_ceiling,
    )?;
    assert_eq!(ranked[0].identity, "existing-host");
    assert_eq!(ranked[0].score, Some(Decimal::from(58)));
    assert_eq!(ranked[1].score, Some(Decimal::from(60)));
    no_ceiling.maximum_total_cost = Some(Decimal::from(59));
    assert_eq!(
        existing_host.estimated_total(no_ceiling.cost_horizon_seconds)?,
        Decimal::from(58)
    );
    let ranked_with_ceiling = rank(&economy, &[new_candidate, existing_candidate], &no_ceiling)?;
    assert_eq!(ranked_with_ceiling[0].identity, "existing-host");
    assert!(ranked_with_ceiling[0].eligible);
    assert!(!ranked_with_ceiling[1].eligible);

    let mut admin_capped = profile()?;
    admin_capped.maximum_total_cost = None;
    admin_capped.administrator_limits.max_total_cost = Some(Decimal::from(5));
    let capped = rank(
        &Policy::Ordered {
            identities: vec!["candidate".to_owned()],
        },
        &[candidate("candidate", Decimal::ONE)],
        &admin_capped,
    )?;
    assert!(!capped[0].eligible);
    assert_eq!(capped[0].reason, "above maximum total cost");
    Ok(())
}

async fn exercise_aws_client(endpoint: &str) -> Result<(), Box<dyn std::error::Error>> {
    let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(aws_credential_types::Credentials::new(
            "local-test",
            "local-secret",
            None,
            None,
            "core-contract",
        ))
        .endpoint_url(endpoint)
        .load()
        .await;
    let ec2 = aws_sdk_ec2::Client::new(&aws_config);
    let run = ec2
        .run_instances()
        .image_id("ami-00000000000000001")
        .instance_type(aws_sdk_ec2::types::InstanceType::T3Small)
        .min_count(1)
        .max_count(1)
        .send()
        .await?;
    assert_eq!(run.instances().len(), 1);
    let hosts = ec2
        .allocate_hosts()
        .availability_zone("us-east-1a")
        .instance_type("mac2.metal")
        .quantity(1)
        .host_recovery(aws_sdk_ec2::types::HostRecovery::On)
        .auto_placement(aws_sdk_ec2::types::AutoPlacement::Off)
        .send()
        .await?;
    assert_eq!(hosts.host_ids(), ["h-00000000000000001"]);

    Ok(())
}

async fn exercise_azure_client(
    endpoint: &str,
    _azure_fixture: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let azure_request = AzureVmRequest {
        location: "westus".to_owned(),
        properties: AzureVmProperties {
            hardware_profile: AzureHardwareProfile {
                vm_size: "Standard_D2s_v3".to_owned(),
            },
            storage_profile: AzureStorageProfile {
                image_reference: AzureImageReference {
                    publisher: "Canonical".to_owned(),
                    offer: "UbuntuServer".to_owned(),
                    sku: "16.04-LTS".to_owned(),
                    version: "latest".to_owned(),
                },
                os_disk: AzureOsDisk {
                    create_option: "FromImage".to_owned(),
                    managed_disk: AzureManagedDisk {
                        storage_account_type: "Premium_LRS".to_owned(),
                    },
                },
            },
            network_profile: AzureNetworkProfile {
                network_interfaces: vec![AzureNetworkInterface {
                    id: "/subscriptions/local/resourceGroups/rg/providers/Microsoft.Network/networkInterfaces/nic"
                        .to_owned(),
                    properties: AzurePrimaryInterface { primary: true },
                }],
            },
        },
    };

    let arm = reqwest::Client::new();
    let create = arm.put(format!("{}/subscriptions/local/resourceGroups/rg/providers/Microsoft.Compute/virtualMachines/runner?api-version=2025-04-01", endpoint))
        .bearer_auth("local-test-token")
        .json(&azure_request)
        .send().await?;
    assert_eq!(create.status(), reqwest::StatusCode::ACCEPTED);
    let operation = create
        .headers()
        .get("Azure-AsyncOperation")
        .ok_or_else(|| io::Error::other("Azure operation header is missing"))?
        .to_str()?
        .to_owned();
    let poll = arm
        .get(format!("{}{operation}", endpoint))
        .bearer_auth("local-test-token")
        .send()
        .await?;
    let operation_result: serde_json::Value = poll.json().await?;
    assert_eq!(operation_result["status"], "Succeeded");

    Ok(())
}

async fn exercise_gcp_client(endpoint: &str) -> Result<(), Box<dyn std::error::Error>> {
    let instances = google_cloud_compute_v1::client::Instances::builder()
        .with_endpoint(format!("{}/compute/v1", endpoint))
        .with_credentials(google_cloud_auth::credentials::anonymous::Builder::new().build())
        .build()
        .await?;
    let gcp_instance = google_cloud_compute_v1::model::Instance::new()
        .set_name("runner-contract")
        .set_machine_type("zones/us-central1-a/machineTypes/e2-standard-2");
    let gcp_operation_result = instances
        .insert()
        .set_project("test-project")
        .set_zone("us-central1-a")
        .set_request_id("00000000-0000-4000-8000-000000000004")
        .set_body(gcp_instance)
        .send()
        .await?;
    assert_eq!(
        gcp_operation_result.name.as_deref(),
        Some("operation-synthetic")
    );

    Ok(())
}

fn assert_client_request_shapes(
    requests: &[CapturedRequest],
    gcp_model: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(requests.len(), 5);
    assert_aws_request_shapes(requests);
    assert_azure_request_shapes(requests)?;
    assert_gcp_request_shape(requests, gcp_model)?;
    Ok(())
}

fn assert_aws_request_shapes(requests: &[CapturedRequest]) {
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].body.contains("Action=RunInstances"));
    assert!(requests[0].body.contains("ImageId=ami-00000000000000001"));
    assert!(requests[0].body.contains("InstanceType=t3.small"));
    assert!(
        requests[0]
            .authorization
            .as_deref()
            .is_some_and(|header| header.starts_with("AWS4-HMAC-SHA256 "))
    );
    assert!(requests[1].body.contains("Action=AllocateHosts"));
    assert!(requests[1].body.contains("AvailabilityZone=us-east-1a"));
    assert!(requests[1].body.contains("InstanceType=mac2.metal"));
    assert!(requests[1].body.contains("HostRecovery=on"));
}

fn assert_azure_request_shapes(
    requests: &[CapturedRequest],
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(requests[2].method, "PUT");
    assert!(
        requests[2]
            .path
            .contains("/virtualMachines/runner?api-version=2025-04-01")
    );
    assert!(
        requests[2]
            .authorization
            .as_deref()
            .is_some_and(|header| header.starts_with("Bearer "))
    );
    let azure_request_body: serde_json::Value = serde_json::from_str(&requests[2].body)?;
    assert_eq!(
        azure_request_body["properties"]["hardwareProfile"]["vmSize"],
        "Standard_D2s_v3"
    );
    assert_eq!(
        azure_request_body["properties"]["storageProfile"]["osDisk"]["createOption"],
        "FromImage"
    );
    assert_eq!(requests[3].method, "GET");
    assert_eq!(requests[3].path, "/operations/op?api-version=2025-04-01");
    Ok(())
}

fn assert_gcp_request_shape(
    requests: &[CapturedRequest],
    gcp_model: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(requests[4].method, "POST");
    assert!(
        requests[4]
            .path
            .contains("/projects/test-project/zones/us-central1-a/instances")
    );
    let gcp_request_body: serde_json::Value = serde_json::from_str(&requests[4].body)?;
    assert_eq!(gcp_request_body["name"], "runner-contract");
    assert_eq!(
        gcp_request_body["machineType"],
        "zones/us-central1-a/machineTypes/e2-standard-2"
    );
    assert!(
        gcp_model["schemas"]["Instance"]["properties"]
            .get("guestAccelerators")
            .is_some()
    );
    assert!(requests[4].authorization.is_none());
    Ok(())
}

#[tokio::test]
async fn real_client_shape_prototypes() {
    let fixture_root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/acceptance/fixtures");
    let ec2_run = std::fs::read_to_string(fixture_root.join("ec2-run.xml")).unwrap();
    let ec2_host = std::fs::read_to_string(fixture_root.join("ec2-host.xml")).unwrap();
    let azure_fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture_root.join("azure-vm.json")).unwrap())
            .unwrap();
    let gcp_model: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture_root.join("gcp-model.json")).unwrap())
            .unwrap();
    let gcp_operation = std::fs::read_to_string(fixture_root.join("gcp-operation.json")).unwrap();
    let server = start_contract_server(vec![
        mock(200, "text/xml", ec2_run),
        mock(200, "text/xml", ec2_host),
        MockResponse {
            status: 202,
            headers: vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                (
                    "Azure-AsyncOperation".to_owned(),
                    "/operations/op?api-version=2025-04-01".to_owned(),
                ),
            ],
            body: azure_fixture.to_string(),
        },
        MockResponse {
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: r#"{"status":"Succeeded"}"#.to_owned(),
        },
        mock(200, "application/json", gcp_operation),
    ])
    .await
    .unwrap();

    exercise_aws_client(&server.endpoint).await.unwrap();
    exercise_azure_client(&server.endpoint, azure_fixture)
        .await
        .unwrap();
    exercise_gcp_client(&server.endpoint).await.unwrap();
    server.task.await.unwrap().unwrap();
    let requests = server.requests.lock().unwrap();
    assert_client_request_shapes(&requests, &gcp_model).unwrap();
}
