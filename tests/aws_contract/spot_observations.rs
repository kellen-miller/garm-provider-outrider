use super::cloud_server::{
    aws_backend, aws_identity, identity_xml, start_aws_contract_server, xml_response,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

use outrider::{
    cloud::aws::AwsBackend,
    config::{AwsCatalogBinding, AwsImageBinding, AwsNetworkBinding},
    providers::{ProviderBinding, PurchasingCapability},
};

#[tokio::test]
async fn spot_request_observation_requires_exact_reason_and_provider_time() {
    let status_response = |request_id: &str, instance_id: &str, code: &str, update_time: &str| {
        xml_response(&format!(
            "<DescribeSpotInstanceRequestsResponse><spotInstanceRequestSet><item><spotInstanceRequestId>{request_id}</spotInstanceRequestId><instanceId>{instance_id}</instanceId><state>closed</state><status><code>{code}</code>{update_time}</status></item></spotInstanceRequestSet></DescribeSpotInstanceRequestsResponse>"
        ))
    };
    let server = start_aws_contract_server(vec![
        identity_xml(),
        status_response(
            "sir-00000000000000001",
            "i-0123456789abcdef0",
            "instance-terminated-no-capacity",
            "<updateTime>2026-10-21T16:00:00Z</updateTime>",
        ),
        status_response(
            "sir-00000000000000001",
            "i-0123456789abcdef0",
            "instance-terminated-by-user",
            "<updateTime>2026-10-21T16:00:00Z</updateTime>",
        ),
        status_response(
            "sir-00000000000000001",
            "i-0123456789abcdef0",
            "instance-terminated-by-price",
            "",
        ),
    ])
    .await
    .unwrap_or_else(|_| panic!("Spot observation server"));
    let backend = aws_backend(&server.endpoint)
        .await
        .unwrap_or_else(|_| panic!("AWS backend"));
    let mut identity = aws_identity("i-0123456789abcdef0");
    identity.dependency_ids.insert(
        "aws:spot-instance-request".to_owned(),
        "sir-00000000000000001".to_owned(),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let evidence = backend
        .observe_interruption(&identity, deadline)
        .await
        .unwrap_or_else(|_| panic!("Spot status"));
    assert_eq!(
        evidence.first().map(|event| event.event_id.as_str()),
        Some("sir-00000000000000001@2026-10-21T16:00:00Z")
    );
    assert!(
        backend
            .observe_interruption(&identity, deadline)
            .await
            .unwrap_or_else(|_| panic!("ordinary termination status"))
            .is_empty()
    );
    assert!(
        backend
            .observe_interruption(&identity, deadline)
            .await
            .expect_err("relevant interruption without provider time is unknown")
            .to_string()
            .contains("no provider update time")
    );
    let requests = server
        .requests
        .lock()
        .unwrap_or_else(|_| panic!("AWS request capture"));
    assert!(requests.iter().skip(1).all(|request| {
        request
            .body
            .contains("SpotInstanceRequestId.1=sir-00000000000000001")
    }));
}

#[tokio::test]
async fn read_only_cloud_calls() {
    let server = start_aws_contract_server(vec![identity_xml()])
        .await
        .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    drop(backend);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].body.contains("Action=GetCallerIdentity"));
    assert!(requests[0].authorization.as_deref().is_some_and(|value| {
        value.starts_with("AWS4-HMAC-SHA256 Credential=AKIDTESTAWSCONTRACT/")
    }));
    assert!(!requests[0].body.contains("RunInstances"));
    server.task.abort();
}

#[test]
fn central_catalog_binding_maps() {
    let provider = ProviderBinding {
        id: "aws-main".to_owned(),
        cloud: "aws".to_owned(),
        account: "synthetic-account".to_owned(),
        location: "us-east-1".to_owned(),
        executable_path: PathBuf::from("/opt/garm/provider/aws"),
        executable_sha256: "a".repeat(64),
        configuration_path: PathBuf::from("/etc/garm/provider/aws.toml"),
        configuration_sha256: "b".repeat(64),
        source_revision: "3c0e79dddd58ea04b95b163014061dd240d52308".to_owned(),
        expected_version: "3c0e79d".to_owned(),
        inherited_environment: Vec::new(),
        supported_os: BTreeSet::from(["linux".to_owned()]),
        supported_architectures: BTreeSet::from(["amd64".to_owned()]),
        purchasing_capabilities: BTreeSet::from([
            PurchasingCapability::OnDemand,
            PurchasingCapability::Spot,
        ]),
    };

    let catalog = AwsCatalogBinding {
        id: "aws-east".to_owned(),
        provider_binding_id: "aws-main".to_owned(),
        account_id: "synthetic-account".to_owned(),
        region: "us-east-1".to_owned(),
        pricing_location: "US East (N. Virginia)".to_owned(),
        credential_reference: "provider-binding:aws-main:configuration".to_owned(),
        locations: BTreeMap::from([(
            "east-a".to_owned(),
            AwsNetworkBinding {
                availability_zone: "us-east-1a".to_owned(),
                subnet_id: "subnet-123".to_owned(),
                security_group_ids: vec!["sg-123".to_owned()],
            },
        )]),
        images: BTreeMap::from([(
            "ubuntu-24".to_owned(),
            AwsImageBinding {
                image_id: "ami-123".to_owned(),
                os: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                capabilities: BTreeSet::from(["docker".to_owned()]),
                windows_license_included: false,
                cloud_init: true,
                ec2_launch_agent: false,
            },
        )]),
        storage_classes: BTreeMap::from([("gp3".to_owned(), "gp3".to_owned())]),
    };

    let settings = AwsBackend::settings_from_bindings(&catalog, &provider, true).unwrap();
    assert_eq!(settings.catalog_id, "aws-east");
    assert_eq!(settings.binding_id, "aws-main");
    assert_eq!(settings.account_id, "synthetic-account");
    assert_eq!(settings.locations["east-a"].security_group_ids, ["sg-123"]);
    assert_eq!(settings.images["ubuntu-24"].image_id, "ami-123");

    let mut mismatched_provider = provider;
    mismatched_provider.location = "us-west-2".to_owned();
    assert!(AwsBackend::settings_from_bindings(&catalog, &mismatched_provider, true).is_err());
}
