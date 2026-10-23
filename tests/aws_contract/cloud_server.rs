use http_body_util::BodyExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    sync::atomic::{AtomicUsize, Ordering},
    sync::{Arc, Mutex},
    time::Duration,
};

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use outrider::{
    cloud::{
        PlacementIdentity,
        aws::{AwsBackend, AwsImage, AwsLocation, AwsSettings},
    },
    config::{ConfigPatch, ResolvedRequest},
    policy::{AwsLaunchFacts, Candidate, CatalogOffer, ProviderLaunchFacts, ProviderSelection},
};

use rust_decimal::Decimal;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

pub(super) const AWS_CHILD_CONFIG: &str = "region = \"us-east-1\"\nsubnet_id = \"subnet-123\"\n[credentials]\ncredential_type = \"static\"\n[credentials.static]\naccess_key_id = \"AKIDTESTAWSCONTRACT\"\nsecret_access_key = \"secret-test-only-not-a-real-key\"\n";

pub(super) fn child_config_sha256() -> String {
    format!("{:x}", Sha256::digest(AWS_CHILD_CONFIG.as_bytes()))
}

pub(super) struct CapturedAwsRequest {
    pub(super) body: String,
    pub(super) authorization: Option<String>,
    pub(super) target: Option<String>,
}

#[derive(serde::Deserialize)]
pub(super) struct AwsPricingFilter {
    #[serde(rename = "Type")]
    pub(super) filter_type: String,
    #[serde(rename = "Field")]
    pub(super) field: String,
    #[serde(rename = "Value")]
    pub(super) value: String,
}

#[derive(serde::Deserialize)]
pub(super) struct AwsPricingRequest {
    #[serde(rename = "ServiceCode")]
    pub(super) service_code: String,
    #[serde(rename = "Filters")]
    pub(super) filters: Vec<AwsPricingFilter>,
}

pub(super) struct RegionalPublicIpv4Quote {
    pub(super) hourly_rate: Option<Decimal>,
    pub(super) request_body: String,
}

pub(super) struct AwsResponse {
    pub(super) status: u16,
    pub(super) body: String,
    pub(super) close: bool,
    pub(super) request_id: Option<String>,
}

pub(super) struct AwsContractServer {
    pub(super) endpoint: String,
    pub(super) requests: Arc<Mutex<Vec<CapturedAwsRequest>>>,
    pub(super) task: tokio::task::JoinHandle<std::io::Result<()>>,
}

pub(super) async fn start_aws_contract_server(
    responses: Vec<AwsResponse>,
) -> std::io::Result<AwsContractServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let task = tokio::spawn(async move {
        for response in responses {
            let (stream, _) = listener.accept().await?;
            if response.close {
                drop(stream);
                continue;
            }

            serve_aws_response(stream, response, recorded.clone()).await?;
        }

        Ok(())
    });
    Ok(AwsContractServer {
        endpoint: format!("http://{address}"),
        requests,
        task,
    })
}

pub(super) async fn serve_aws_response(
    stream: tokio::net::TcpStream,
    response: AwsResponse,
    recorded: Arc<Mutex<Vec<CapturedAwsRequest>>>,
) -> std::io::Result<()> {
    let service = service_fn(move |request: hyper::Request<Incoming>| {
        let recorded = recorded.clone();
        let status = response.status;
        let response_body = response.body.clone();
        let request_id = response.request_id.clone();
        async move {
            let (parts, request_body) = request.into_parts();
            let bytes = request_body
                .collect()
                .await
                .map_err(std::io::Error::other)?
                .to_bytes();
            let captured = CapturedAwsRequest {
                body: String::from_utf8_lossy(&bytes).into_owned(),
                authorization: parts
                    .headers
                    .get(hyper::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                target: parts
                    .headers
                    .get("x-amz-target")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
            };
            let accepts_regional_usage = regional_public_ipv4_request_matches(&captured.body);

            recorded
                .lock()
                .map_err(|_| std::io::Error::other("AWS contract request lock is poisoned"))?
                .push(captured);
            let mut builder = hyper::Response::builder()
                .status(status)
                .header("content-type", "text/xml")
                .header("connection", "close");
            if let Some(request_id) = request_id {
                builder = builder.header("x-amzn-requestid", request_id);
            }

            let response_body =
                if let Some(scenario) = response_body.strip_prefix("regional-public-ipv4:") {
                    if accepts_regional_usage {
                        json!({"PriceList": regional_public_ipv4_products(scenario)}).to_string()
                    } else {
                        json!({"PriceList": []}).to_string()
                    }
                } else {
                    response_body
                };

            builder
                .body(http_body_util::Full::new(bytes::Bytes::from(response_body)))
                .map_err(std::io::Error::other)
        }
    });
    http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
        .map_err(std::io::Error::other)
}

pub(super) fn regional_public_ipv4_request_matches(body: &str) -> bool {
    let Ok(request) = serde_json::from_str::<AwsPricingRequest>(body) else {
        return false;
    };
    if request.service_code != "AmazonVPC" || request.filters.len() != 3 {
        return false;
    }

    let filters = request
        .filters
        .into_iter()
        .map(|filter| (filter.field, (filter.filter_type, filter.value)))
        .collect::<BTreeMap<_, _>>();
    filters
        == BTreeMap::from([
            (
                "location".to_owned(),
                ("TERM_MATCH".to_owned(), "Europe (Ireland)".to_owned()),
            ),
            (
                "regionCode".to_owned(),
                ("TERM_MATCH".to_owned(), "eu-west-1".to_owned()),
            ),
            (
                "group".to_owned(),
                ("TERM_MATCH".to_owned(), "VPCPublicIPv4Address".to_owned()),
            ),
        ])
}

pub(super) fn regional_public_ipv4_products(scenario: &str) -> Vec<String> {
    let Ok(mut product) = serde_json::from_str::<Value>(include_str!(
        "../fixtures/aws/eu-west-1-public-ipv4-product.json"
    )) else {
        panic!("regional public IPv4 fixture must be valid JSON");
    };
    match scenario {
        "valid" => vec![product.to_string()],
        "wrong-region" => {
            product["product"]["attributes"]["regionCode"] = json!("us-east-1");
            vec![product.to_string()]
        }
        "idle" => {
            product["product"]["attributes"]["usagetype"] = json!("EUW1-PublicIPv4:IdleAddress");
            vec![product.to_string()]
        }
        "ambiguous" => {
            let mut duplicate = product.clone();
            duplicate["product"]["sku"] = json!("eu-west-1-public-ipv4-duplicate");
            duplicate["product"]["attributes"]["sku"] = json!("eu-west-1-public-ipv4-duplicate");
            duplicate["terms"]["OnDemand"]["offer"]["priceDimensions"]["dimension"]["pricePerUnit"]
                ["USD"] = json!("0.006");
            vec![product.to_string(), duplicate.to_string()]
        }
        _ => Vec::new(),
    }
}

pub(super) async fn aws_backend(endpoint: &str) -> Result<AwsBackend, outrider::cloud::CloudError> {
    aws_backend_with_settings(endpoint, aws_settings(endpoint)).await
}

pub(super) fn aws_settings(endpoint: &str) -> AwsSettings {
    static NEXT_CONFIG_ID: AtomicUsize = AtomicUsize::new(0);
    let config_path = std::env::temp_dir().join(format!(
        "outrider-aws-contract-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG_ID.fetch_add(1, Ordering::Relaxed)
    ));
    assert!(fs::write(&config_path, AWS_CHILD_CONFIG).is_ok());
    AwsSettings {
        catalog_id: "aws-east".to_owned(),
        account_id: "synthetic-account".to_owned(),
        region: "us-east-1".to_owned(),
        pricing_location: "US East (N. Virginia)".to_owned(),
        locations: BTreeMap::from([(
            "us-east-1".to_owned(),
            AwsLocation {
                region: "us-east-1".to_owned(),
                availability_zone: "us-east-1a".to_owned(),
                subnet_id: "subnet-123".to_owned(),
                security_group_ids: vec!["sg-123".to_owned()],
            },
        )]),
        images: BTreeMap::from([(
            "ubuntu-24".to_owned(),
            AwsImage {
                image_id: "ami-123".to_owned(),
                os: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                capabilities: BTreeSet::from(["docker".to_owned()]),
                windows_license_included: false,
                cloud_init: true,
                ec2_launch_agent: false,
            },
        )]),
        storage_classes: BTreeMap::from([
            ("gp3".to_owned(), "gp3".to_owned()),
            ("ssd".to_owned(), "gp3".to_owned()),
        ]),
        include_spot_placement_scores: false,
        binding_id: "aws-main".to_owned(),
        source_revision: "3c0e79dddd58ea04b95b163014061dd240d52308".to_owned(),
        binary_sha256: "a".repeat(64),
        configuration_sha256: child_config_sha256(),
        ec2_endpoint_url: Some(endpoint.to_owned()),
        pricing_endpoint_url: Some(endpoint.to_owned()),
        sts_endpoint_url: Some(endpoint.to_owned()),
        provider_configuration_path: config_path,
    }
}

pub(super) async fn aws_backend_with_settings(
    endpoint: &str,
    settings: AwsSettings,
) -> Result<AwsBackend, outrider::cloud::CloudError> {
    let shared = aws_config::defaults(BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            "AKIDTESTAWSCONTRACT",
            "secret-test-only-not-a-real-key",
            None,
            None,
            "aws-contract-test",
        ))
        .load()
        .await;
    let ec2 = aws_sdk_ec2::Client::from_conf(
        aws_sdk_ec2::config::Builder::from(&shared)
            .endpoint_url(endpoint)
            .build(),
    );
    let pricing = aws_sdk_pricing::Client::from_conf(
        aws_sdk_pricing::config::Builder::from(&shared)
            .region(aws_config::Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .build(),
    );
    let sts = aws_sdk_sts::Client::from_conf(
        aws_sdk_sts::config::Builder::from(&shared)
            .endpoint_url(endpoint)
            .build(),
    );
    AwsBackend::with_clients(
        settings,
        ec2,
        pricing,
        sts,
        tokio::time::Instant::now() + Duration::from_secs(5),
    )
    .await
}

pub(super) fn resolved_profile() -> Result<ResolvedRequest, Box<dyn std::error::Error>> {
    let patch = ConfigPatch::from_yaml(include_str!("../../examples/central.yaml"))?;
    let mut resolved = patch.normalize_profile(None, "linux-large", None)?;
    resolved.profile.boot_disk.class = "ssd".to_owned();
    resolved.profile.boot_disk.iops = Some(4000);
    resolved.profile.boot_disk.throughput_mib_s = Some(225);
    Ok(resolved)
}

pub(super) fn candidate(purchasing: &str) -> CatalogOffer {
    let candidate = Candidate {
        identity: format!(
            "aws|synthetic-account|us-east-1|us-east-1|m7i.xlarge|ami-123|{purchasing}"
        ),
        os: "linux".to_owned(),
        architecture: "amd64".to_owned(),
        image: "ubuntu-24".to_owned(),
        vcpu: 8,
        memory_gib: Decimal::from(16),
        machine_type: "m7i.xlarge".to_owned(),
        machine_family: Some("m7i".to_owned()),
        cloud: "aws".to_owned(),
        purchasing: purchasing.to_owned(),
        account: "synthetic-account".to_owned(),
        location: "us-east-1".to_owned(),
        gpu_vendor: None,
        gpu_model: None,
        gpu_count: 0,
        gpu_memory_gib: None,
        boot_disk_class: "ssd".to_owned(),
        boot_disk_gib: 40,
        boot_disk_iops: 3000,
        boot_disk_throughput_mib_s: Decimal::from(125u32),
        local_scratch_gib: Decimal::ZERO,
        capabilities: vec!["docker".to_owned()],
        price: None,
        interruption_rate_per_hour: None,
        allocation_failure_rate: None,
        mean_create_seconds: None,
        observation_age_seconds: None,
        observation_ages: None,
        observation_window: None,
        observation_samples: 0,
        definitive_create_outcomes: 0,
        measured_latency_samples: 0,
        interruption_samples: 0,
        native_capacity_hint: None,
    };

    CatalogOffer {
        candidate,
        selection: ProviderSelection {
            binding_id: "aws-main".to_owned(),
            source_revision: "3c0e79dddd58ea04b95b163014061dd240d52308".to_owned(),
            binary_sha256: "a".repeat(64),
            configuration_sha256: child_config_sha256(),
            flavor: "m7i.xlarge".to_owned(),
            image: "ami-123".to_owned(),
            approved_extras: BTreeMap::from([
                ("spot".to_owned(), json!(purchasing == "spot")),
                ("volume_type".to_owned(), json!("gp3")),
                ("volume_size".to_owned(), json!(40)),
                ("iops".to_owned(), json!(3000)),
                ("throughput".to_owned(), json!(125)),
            ]),
        },
        launch_facts: ProviderLaunchFacts::Aws(AwsLaunchFacts {
            availability_zone: "us-east-1a".to_owned(),
            subnet_id: "subnet-123".to_owned(),
            map_public_ip_on_launch: true,
            root_device_name: "/dev/xvda".to_owned(),
            root_volume_minimum_gib: 8,
            maximum_ebs_iops: 3000,
            maximum_ebs_throughput_mib_s: Decimal::from(125u32),
        }),
    }
}

pub(super) fn xml_response(body: &str) -> AwsResponse {
    AwsResponse {
        status: 200,
        body: body.to_owned(),
        close: false,
        request_id: None,
    }
}

pub(super) fn identity_xml() -> AwsResponse {
    xml_response(
        "<GetCallerIdentityResponse><GetCallerIdentityResult><Account>synthetic-account</Account><Arn>arn:aws:iam::synthetic-account:user/test</Arn><UserId>test</UserId></GetCallerIdentityResult></GetCallerIdentityResponse>",
    )
}

pub(super) fn price_product(attributes: Value, unit: &str, rate: &str) -> String {
    json!({
        "product": {
            "sku": attributes["sku"],
            "productFamily": attributes["productFamily"],
            "attributes": attributes
        },
        "serviceCode": "AmazonEC2",
        "terms": {"OnDemand": {"offer": {
            "effectiveDate": "2026-01-01T00:00:00Z",
            "priceDimensions": {"dimension": {
                "unit": unit,
                "beginRange": "0",
                "endRange": "Inf",
                "appliesTo": [],
                "pricePerUnit": {"USD": rate}
            }}
        }}}
    })
    .to_string()
}

pub(super) fn pricing_response(products: Vec<String>) -> AwsResponse {
    AwsResponse {
        status: 200,
        body: json!({"PriceList": products}).to_string(),
        close: false,
        request_id: Some("pricing-request".to_owned()),
    }
}

pub(super) fn compute_product() -> String {
    price_product(
        json!({
            "sku": "compute-sku",
            "productFamily": "Compute Instance",
            "servicecode": "AmazonEC2",
            "instanceType": "m7i.xlarge",
            "location": "US East (N. Virginia)",
            "regionCode": "us-east-1",
            "operatingSystem": "Linux",
            "operation": "RunInstances",
            "usagetype": "USE1-BoxUsage:m7i.xlarge",
            "preInstalledSw": "NA",
            "tenancy": "Shared",
            "capacitystatus": "Used"
        }),
        "Hrs",
        "0.10",
    )
}

pub(super) fn disk_products() -> Vec<String> {
    vec![
        price_product(
            json!({
                "sku": "storage-sku",
                "productFamily": "Storage",
                "location": "US East (N. Virginia)",
                "regionCode": "us-east-1",
                "volumeApiName": "gp3",
                "usagetype": "USE1-EBS:VolumeUsage.gp3"
            }),
            "GB-Mo",
            "0.073",
        ),
        price_product(
            json!({
                "sku": "iops-sku",
                "productFamily": "Provisioned IOPS",
                "volumeApiName": "gp3",
                "location": "US East (N. Virginia)",
                "regionCode": "us-east-1",
                "volumeApiName": "gp3",
                "usagetype": "USE1-EBS:VolumeP-IOPS.gp3"
            }),
            "IOPS-Mo",
            "0.0073",
        ),
        price_product(
            json!({
                "sku": "throughput-sku",
                "productFamily": "Provisioned Throughput",
                "volumeApiName": "gp3",
                "location": "US East (N. Virginia)",
                "regionCode": "us-east-1",
                "volumeApiName": "gp3",
                "usagetype": "USE1-EBS:VolumeP-Throughput.gp3"
            }),
            "MBps-Mo",
            "0.073",
        ),
    ]
}

pub(super) fn ipv4_product() -> String {
    price_product(
        json!({
            "sku": "ipv4-sku",
            "location": "US East (N. Virginia)",
            "regionCode": "us-east-1",
            "group": "VPCPublicIPv4Address",
            "usagetype": "USE1-PublicIPv4:InUseAddress"
        }),
        "Hrs",
        "0.005",
    )
}

pub(super) fn subnet_xml() -> &'static str {
    "<DescribeSubnetsResponse><subnetSet><item><subnetId>subnet-123</subnetId><availabilityZone>us-east-1a</availabilityZone><mapPublicIpOnLaunch>true</mapPublicIpOnLaunch></item></subnetSet></DescribeSubnetsResponse>"
}

pub(super) fn image_xml() -> &'static str {
    "<DescribeImagesResponse><imagesSet><item><imageId>ami-123</imageId><imageState>available</imageState><architecture>x86_64</architecture><platformDetails>Linux/UNIX</platformDetails><usageOperation>RunInstances</usageOperation><rootDeviceName>/dev/xvda</rootDeviceName><blockDeviceMapping><item><deviceName>/dev/xvda</deviceName><ebs><snapshotId>snap-123</snapshotId><volumeSize>32</volumeSize><deleteOnTermination>true</deleteOnTermination></ebs></item></blockDeviceMapping></item></imagesSet></DescribeImagesResponse>"
}

pub(super) fn instance_type_xml() -> &'static str {
    "<DescribeInstanceTypesResponse><instanceTypeSet><item><instanceType>m7i.xlarge</instanceType><processorInfo><supportedArchitectures><item>x86_64</item></supportedArchitectures></processorInfo><vCpuInfo><defaultVCpus>8</defaultVCpus></vCpuInfo><memoryInfo><sizeInMiB>16384</sizeInMiB></memoryInfo><ebsInfo><ebsOptimizedSupport>default</ebsOptimizedSupport><ebsOptimizedInfo><maximumIops>5000</maximumIops><maximumThroughputInMBps>250.0</maximumThroughputInMBps></ebsOptimizedInfo></ebsInfo><instanceStorageSupported>false</instanceStorageSupported></item></instanceTypeSet><nextToken>types-next</nextToken></DescribeInstanceTypesResponse>"
}

pub(super) fn instance_type_xml_with_throughput(
    maximum_throughput_mbps: Option<&str>,
    optimized_support: &str,
    maximum_iops: Option<u32>,
) -> String {
    let ebs_iops = maximum_iops.map_or_else(String::new, |maximum| {
        format!("<maximumIops>{maximum}</maximumIops>")
    });
    let ebs_info = maximum_throughput_mbps.map_or_else(String::new, |maximum| {
        format!("<ebsInfo><ebsOptimizedSupport>{optimized_support}</ebsOptimizedSupport><ebsOptimizedInfo>{ebs_iops}<maximumThroughputInMBps>{maximum}</maximumThroughputInMBps></ebsOptimizedInfo></ebsInfo>")
    });
    format!(
        "<DescribeInstanceTypesResponse><instanceTypeSet><item><instanceType>m7i.xlarge</instanceType><processorInfo><supportedArchitectures><item>x86_64</item></supportedArchitectures></processorInfo><vCpuInfo><defaultVCpus>8</defaultVCpus></vCpuInfo><memoryInfo><sizeInMiB>16384</sizeInMiB></memoryInfo>{ebs_info}<instanceStorageSupported>false</instanceStorageSupported></item></instanceTypeSet></DescribeInstanceTypesResponse>"
    )
}

pub(super) fn image_xml_with_root_volume_size(size_gib: u32) -> String {
    image_xml().replace(
        "<volumeSize>32</volumeSize>",
        &format!("<volumeSize>{size_gib}</volumeSize>"),
    )
}

pub(super) fn offering_xml() -> &'static str {
    "<DescribeInstanceTypeOfferingsResponse><instanceTypeOfferingSet><item><instanceType>m7i.xlarge</instanceType><location>us-east-1a</location><locationType>availability-zone</locationType></item></instanceTypeOfferingSet><nextToken>offers-next</nextToken></DescribeInstanceTypeOfferingsResponse>"
}

pub(super) fn instance_item_xml(
    id: &str,
    name: &str,
    controller: Option<&str>,
    pool: Option<&str>,
    subnet: &str,
    zone: &str,
) -> String {
    let controller_tag = controller.map_or_else(String::new, |value| {
        format!("<item><key>GARM_CONTROLLER_ID</key><value>{value}</value></item>")
    });
    let pool_tag = pool.map_or_else(String::new, |value| {
        format!("<item><key>GARM_POOL_ID</key><value>{value}</value></item>")
    });
    format!(
        "<item><instanceId>{id}</instanceId><instanceState><name>running</name></instanceState><placement><availabilityZone>{zone}</availabilityZone></placement><subnetId>{subnet}</subnetId><tagSet><item><key>Name</key><value>{name}</value></item>{controller_tag}{pool_tag}</tagSet></item>"
    )
}

pub(super) fn instances_xml(instances: &[String], next_token: Option<&str>) -> String {
    let next_token = next_token.map_or_else(String::new, |token| {
        format!("<nextToken>{token}</nextToken>")
    });
    format!(
        "<DescribeInstancesResponse><reservationSet><item><instancesSet>{}</instancesSet></item></reservationSet>{next_token}</DescribeInstancesResponse>",
        instances.join("")
    )
}

pub(super) fn aws_identity(resource_id: &str) -> PlacementIdentity {
    PlacementIdentity {
        controller_id: "controller-1".to_owned(),
        cloud: "aws".to_owned(),
        account_id: "synthetic-account".to_owned(),
        location: "us-east-1".to_owned(),
        resource_id: resource_id.to_owned(),
        host_id: None,
        dependency_ids: BTreeMap::new(),
    }
}
