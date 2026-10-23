use super::cloud_server::{
    AWS_CHILD_CONFIG, AwsResponse, CapturedAwsRequest, aws_backend, aws_backend_with_settings,
    aws_identity, aws_settings, compute_product, disk_products, identity_xml, image_xml,
    instance_item_xml, instance_type_xml, instances_xml, ipv4_product, offering_xml,
    pricing_response, resolved_profile, start_aws_contract_server, subnet_xml, xml_response,
};
use std::{fs, time::Duration};

use outrider::{
    cloud::{CatalogQuery, CloudCatalog, OwnershipScope, aws::AwsBackend},
    policy::ProviderLaunchFacts,
};

use rust_decimal::Decimal;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[tokio::test]
async fn binding_account_region_mismatch() {
    let server = start_aws_contract_server(vec![identity_xml()])
        .await
        .unwrap();
    let mut settings = aws_settings(&server.endpoint);
    let changed_child_config = AWS_CHILD_CONFIG.replace("us-east-1", "us-west-2");
    fs::write(&settings.provider_configuration_path, &changed_child_config).unwrap();
    settings.configuration_sha256 = hex::encode(Sha256::digest(changed_child_config.as_bytes()));
    let error = aws_backend_with_settings(&server.endpoint, settings)
        .await
        .err()
        .unwrap();
    assert!(matches!(
        error,
        outrider::cloud::CloudError::Failure(failure) if failure.kind == "configuration"
    ));
    assert!(server.requests.lock().unwrap().is_empty());
    server.task.abort();

    let server = start_aws_contract_server(vec![xml_response(
        "<GetCallerIdentityResponse><GetCallerIdentityResult><Account>other-account</Account><Arn>arn:aws:iam::other-account:user/test</Arn><UserId>test</UserId></GetCallerIdentityResult></GetCallerIdentityResponse>",
    )])
    .await
    .unwrap();
    let error = aws_backend(&server.endpoint).await.err().unwrap();
    assert!(matches!(
        error,
        outrider::cloud::CloudError::Failure(failure) if failure.kind == "ownership"
    ));
    server.task.abort();

    let server = start_aws_contract_server(Vec::new()).await.unwrap();
    let mut settings = aws_settings(&server.endpoint);
    let role_config = "region = \"us-east-1\"\nsubnet_id = \"subnet-123\"\n[credentials]\ncredential_type = \"role\"\n";
    fs::write(&settings.provider_configuration_path, role_config).unwrap();
    settings.configuration_sha256 = hex::encode(Sha256::digest(role_config.as_bytes()));
    let error = AwsBackend::connect(
        settings,
        tokio::time::Instant::now() + Duration::from_secs(5),
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(
        error,
        outrider::cloud::CloudError::Failure(failure) if failure.kind == "unsupported"
    ));
    server.task.abort();
}

#[tokio::test]
async fn unrestricted_inventory_keeps_page_size() {
    let server = start_aws_contract_server(vec![
        identity_xml(),
        xml_response(image_xml()),
        xml_response(subnet_xml()),
        xml_response("<DescribeInstanceTypesResponse/>"),
        xml_response("<DescribeInstanceTypeOfferingsResponse/>"),
    ])
    .await
    .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let mut profile = resolved_profile().unwrap().profile;
    profile.machine_types.clear();
    let discovery = backend
        .catalog(
            CatalogQuery {
                clouds: vec!["aws".to_owned()],
                accounts: vec!["synthetic-account".to_owned()],
                locations: vec!["us-east-1".to_owned()],
                profile,
            },
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(discovery.offers.is_empty());
    let requests = server.requests.lock().unwrap();
    assert!(requests[3].body.contains("MaxResults=100"));
    assert!(!requests[3].body.contains("InstanceType.1="));
    server.task.abort();
}

#[tokio::test]
async fn paginated_inventory_prices() {
    let price_case: serde_json::Value = serde_json::from_str(include_str!(
        "../acceptance/fixtures/aws-price-components.json"
    ))
    .unwrap();
    let candidate_case = &price_case["candidate"];
    let hourly = &price_case["expected_hourly"];
    let server = start_aws_contract_server(vec![
        identity_xml(),
        xml_response(image_xml()),
        xml_response(subnet_xml()),
        xml_response(instance_type_xml()),
        xml_response("<DescribeInstanceTypesResponse/>"),
        xml_response(offering_xml()),
        xml_response("<DescribeInstanceTypeOfferingsResponse/>"),
        xml_response(subnet_xml()),
        xml_response(image_xml()),
        pricing_response(vec![compute_product()]),
        pricing_response(disk_products()),
        pricing_response(vec![ipv4_product()]),
    ])
    .await
    .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let profile = resolved_profile().unwrap().profile;
    let result = backend
        .catalog(
            CatalogQuery {
                clouds: vec!["aws".to_owned()],
                accounts: vec!["synthetic-account".to_owned()],
                locations: vec!["us-east-1".to_owned()],
                profile,
            },
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await;
    let candidates = result.unwrap().offers;
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].candidate.machine_type, "m7i.xlarge");
    assert_eq!(candidates[0].candidate.memory_gib, Decimal::from(16));
    assert_eq!(candidates[0].candidate.local_scratch_gib, Decimal::ZERO);
    assert_eq!(
        candidates[0].candidate.boot_disk_throughput_mib_s,
        Decimal::from(225u32)
    );
    let ProviderLaunchFacts::Aws(launch_facts) = &candidates[0].launch_facts else {
        panic!("expected AWS launch facts")
    };
    assert_eq!(
        launch_facts.maximum_ebs_throughput_mib_s,
        Decimal::from_str_exact("238.4185791015625").unwrap()
    );
    let price_candidate = candidates.first().unwrap();
    assert_eq!(
        price_candidate.candidate.boot_disk_gib,
        candidate_case["boot_disk_gib"].as_u64().unwrap() as u32
    );
    assert_eq!(
        price_candidate.candidate.boot_disk_iops,
        candidate_case["boot_disk_iops"].as_u64().unwrap() as u32
    );
    assert_eq!(
        price_candidate.selection.approved_extras["throughput"],
        json!(
            candidate_case["boot_disk_throughput_mib_s"]
                .as_u64()
                .unwrap()
        )
    );
    let quote = backend
        .quote(
            price_candidate,
            Duration::from_secs(candidate_case["duration_seconds"].as_u64().unwrap()),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(
        quote.compute.hourly_rate.unwrap(),
        hourly["compute"]
            .as_str()
            .unwrap()
            .parse::<Decimal>()
            .unwrap()
    );
    assert_eq!(
        quote.disk.hourly_rate.unwrap(),
        hourly["disk_total"]
            .as_str()
            .unwrap()
            .parse::<Decimal>()
            .unwrap()
    );
    assert_eq!(
        quote.public_address.hourly_rate.unwrap(),
        hourly["public_ipv4"]
            .as_str()
            .unwrap()
            .parse::<Decimal>()
            .unwrap()
    );
    assert_eq!(
        quote.compute.hourly_rate.unwrap()
            + quote.disk.hourly_rate.unwrap()
            + quote.public_address.hourly_rate.unwrap(),
        hourly["total"]
            .as_str()
            .unwrap()
            .parse::<Decimal>()
            .unwrap()
    );
    assert!(quote.compute.observed_at_unix_seconds.is_some());
    assert_eq!(quote.compute.effective_at_unix_seconds, Some(1_767_225_600));
    let requests = server.requests.lock().unwrap();
    assert_paginated_catalog_requests(&requests);
    assert_eq!(price_candidate.candidate.boot_disk_class, "ssd");
    server.task.abort();
}

fn assert_paginated_catalog_requests(requests: &[CapturedAwsRequest]) {
    assert_eq!(requests.len(), 12);
    assert!(requests[4].body.contains("NextToken=types-next"));
    assert!(requests[6].body.contains("NextToken=offers-next"));
    assert!(requests[3].body.contains("InstanceType.1=m7i.xlarge"));
    assert!(!requests[3].body.contains("MaxResults="));
    assert!(!requests[4].body.contains("MaxResults="));
    assert!(
        requests[9]
            .target
            .as_deref()
            .is_some_and(|target| target.ends_with("GetProducts"))
    );
    assert!(requests[9].body.contains("operatingSystem"));
    assert!(requests[10].body.contains("volumeApiName"));
    assert!(requests[10].body.contains("location"));
    assert!(requests[11].body.contains("VPCPublicIPv4Address"));
    assert!(requests[11].body.contains("AmazonVPC"));
    assert!(requests[11].body.contains("regionCode"));
}

#[tokio::test]
async fn regional_compute_rate_cannot_cross_regions() {
    let mut product: Value = serde_json::from_str(&compute_product()).unwrap();
    product["product"]["attributes"]["regionCode"] = json!("us-west-2");
    let server = start_aws_contract_server(vec![
        identity_xml(),
        xml_response(image_xml()),
        xml_response(subnet_xml()),
        xml_response(instance_type_xml()),
        xml_response("<DescribeInstanceTypesResponse/>"),
        xml_response(offering_xml()),
        xml_response("<DescribeInstanceTypeOfferingsResponse/>"),
        xml_response(subnet_xml()),
        xml_response(image_xml()),
        pricing_response(vec![product.to_string()]),
        pricing_response(disk_products()),
        pricing_response(vec![ipv4_product()]),
    ])
    .await
    .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let candidates = backend
        .catalog(
            CatalogQuery {
                clouds: vec!["aws".to_owned()],
                accounts: vec!["synthetic-account".to_owned()],
                locations: vec!["us-east-1".to_owned()],
                profile: resolved_profile().unwrap().profile,
            },
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap()
        .offers;
    let offer = candidates
        .iter()
        .find(|offer| offer.candidate.purchasing == "on_demand")
        .unwrap();
    let quote = backend
        .quote(
            offer,
            Duration::from_secs(3600),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(quote.compute.hourly_rate, None);
    assert_eq!(
        quote.disk.hourly_rate,
        Some(Decimal::from_str_exact("0.0240").unwrap())
    );
    let requests = server.requests.lock().unwrap();
    assert!(requests[9].body.contains("regionCode"));
    assert!(requests[10].body.contains("regionCode"));
    server.task.abort();
}

#[tokio::test]
async fn scoped_capacity_advice() {
    let server = start_aws_contract_server(vec![
        identity_xml(),
        xml_response(image_xml()),
        xml_response(subnet_xml()),
        xml_response(instance_type_xml()),
        xml_response("<DescribeInstanceTypesResponse/>"),
        xml_response(offering_xml()),
        xml_response("<DescribeInstanceTypeOfferingsResponse/>"),
        xml_response("<DescribeAvailabilityZonesResponse><availabilityZoneInfo><item><zoneName>us-east-1a</zoneName><zoneId>use1-az1</zoneId><regionName>us-east-1</regionName><zoneState>available</zoneState></item></availabilityZoneInfo></DescribeAvailabilityZonesResponse>"),
        xml_response("<GetSpotPlacementScoresResponse><spotPlacementScoreSet><item><region>us-east-1</region><availabilityZoneId>use1-az1</availabilityZoneId><score>7</score></item></spotPlacementScoreSet></GetSpotPlacementScoresResponse>"),
    ])
    .await.unwrap();
    let mut settings = aws_settings(&server.endpoint);
    settings.include_spot_placement_scores = true;
    let backend = aws_backend_with_settings(&server.endpoint, settings)
        .await
        .unwrap();
    let mut profile = resolved_profile().unwrap().profile;
    profile.purchasing_models.insert("spot".to_owned());
    let candidates = backend
        .catalog(
            CatalogQuery {
                clouds: vec!["aws".to_owned()],
                accounts: vec!["synthetic-account".to_owned()],
                locations: vec!["us-east-1".to_owned()],
                profile,
            },
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap()
        .offers;
    assert_eq!(candidates.len(), 2);
    let spot = candidates
        .iter()
        .find(|offer| offer.candidate.purchasing == "spot")
        .unwrap();
    let hint = spot.candidate.native_capacity_hint.as_ref().unwrap();
    assert_eq!(
        hint.source,
        "aws:GetSpotPlacementScores;scope=availability-zone;target=1-unit;types=m7i.xlarge"
    );
    assert_eq!(hint.cloud, "aws");
    assert_eq!(hint.account, "synthetic-account");
    assert_eq!(hint.location, "us-east-1");
    assert!(hint.observed_at_unix_seconds > 0);
    assert_eq!(hint.score, Decimal::from(7));
    assert!(
        candidates
            .iter()
            .find(|offer| offer.candidate.purchasing == "on_demand")
            .unwrap()
            .candidate
            .native_capacity_hint
            .is_none()
    );
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 9);
    assert!(requests[8].body.contains("Action=GetSpotPlacementScores"));
    assert!(requests[8].body.contains("TargetCapacity=1"));
    assert!(requests[8].body.contains("TargetCapacityUnitType=units"));
    assert!(requests[8].body.contains("SingleAvailabilityZone=true"));
    server.task.abort();
}

#[tokio::test]
async fn provider_ownership_observations() {
    let requested = instance_item_xml(
        "i-0123456789abcdef0",
        "runner-1",
        Some("controller-1"),
        Some("pool-1"),
        "subnet-123",
        "us-east-1a",
    );
    let other_name = instance_item_xml(
        "i-0123456789abcdef1",
        "runner-2",
        Some("controller-1"),
        Some("pool-2"),
        "subnet-123",
        "us-east-1a",
    );
    let foreign_controller = instance_item_xml(
        "i-0123456789abcdef2",
        "runner-1",
        Some("controller-2"),
        Some("pool-1"),
        "subnet-123",
        "us-east-1a",
    );
    let server = start_aws_contract_server(vec![
        identity_xml(),
        xml_response(&instances_xml(
            std::slice::from_ref(&requested),
            Some("next-page"),
        )),
        xml_response(&instances_xml(&[other_name, foreign_controller], None)),
    ])
    .await
    .unwrap();
    let mut settings = aws_settings(&server.endpoint);
    settings.locations.insert(
        "same-placement-alias".to_owned(),
        settings.locations["us-east-1"].clone(),
    );
    let backend = aws_backend_with_settings(&server.endpoint, settings)
        .await
        .unwrap();
    let listed = backend
        .list(
            &OwnershipScope {
                controller_id: "controller-1".to_owned(),
                resource_name: Some("runner-1".to_owned()),
            },
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "runner-1");
    assert_eq!(listed[0].identity.resource_id, "i-0123456789abcdef0");
    assert_eq!(listed[0].identity.location, "us-east-1");
    assert_eq!(listed[0].identity.controller_id, "controller-1");
    assert!(
        listed[0]
            .ownership_tags
            .iter()
            .any(|(key, value)| { key == "GARM_POOL_ID" && value == "pool-1" })
    );
    assert!(
        !listed[0]
            .ownership_tags
            .iter()
            .any(|(key, _)| key.starts_with("outrider:"))
    );
    {
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].body.contains("tag%3AGARM_CONTROLLER_ID"));
        assert!(requests[2].body.contains("NextToken=next-page"));
        assert!(requests[1].body.contains("tag%3AName"));
    }
    server.task.abort();

    let server = start_aws_contract_server(vec![
        identity_xml(),
        xml_response(&instances_xml(&[requested], None)),
    ])
    .await
    .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let inspected = backend
        .inspect(
            &aws_identity("i-0123456789abcdef0"),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inspected.identity.resource_id, "i-0123456789abcdef0");
    assert_eq!(inspected.identity.location, "us-east-1");
    {
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .body
                .contains("InstanceId.1=i-0123456789abcdef0")
        );
        assert!(!requests[1].body.contains("client-token"));
    }
    let error = backend
        .inspect(
            &aws_identity("client-token:legacy-token"),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        outrider::cloud::CloudError::Failure(failure) if failure.kind == "ownership"
    ));
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    server.task.abort();

    let server = start_aws_contract_server(vec![identity_xml(), AwsResponse {
        status: 400,
        body: "<Response><Errors><Error><Code>InvalidInstanceID.NotFound</Code><Message>not found</Message></Error></Errors><RequestID>not-found</RequestID></Response>".to_owned(),
        close: false,
        request_id: Some("not-found".to_owned()),
    }])
    .await
    .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    assert!(
        backend
            .inspect(
                &aws_identity("i-0123456789abcdef9"),
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap()
            .is_none()
    );
    server.task.abort();

    let server = start_aws_contract_server(vec![
        identity_xml(),
        AwsResponse {
            status: 500,
            body: "<Response><Errors><Error><Code>InternalError</Code><Message>service unavailable</Message></Error></Errors><RequestID>unavailable</RequestID></Response>".to_owned(),
            close: false,
            request_id: Some("unavailable".to_owned()),
        },
        AwsResponse {
            status: 500,
            body: "<Response><Errors><Error><Code>InternalError</Code><Message>service unavailable</Message></Error></Errors><RequestID>unavailable</RequestID></Response>".to_owned(),
            close: false,
            request_id: Some("unavailable".to_owned()),
        },
        AwsResponse {
            status: 500,
            body: "<Response><Errors><Error><Code>InternalError</Code><Message>service unavailable</Message></Error></Errors><RequestID>unavailable</RequestID></Response>".to_owned(),
            close: false,
            request_id: Some("unavailable".to_owned()),
        },
    ])
    .await
    .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let error = backend
        .inspect(
            &aws_identity("i-0123456789abcdef0"),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        outrider::cloud::CloudError::Failure(failure) if failure.kind == "aws-InternalError"
    ));
    server.task.abort();

    let malformed = instance_item_xml(
        "i-0123456789abcdef3",
        "runner-3",
        Some("controller-1"),
        None,
        "subnet-123",
        "us-east-1a",
    );
    let server = start_aws_contract_server(vec![
        identity_xml(),
        xml_response(&instances_xml(&[malformed], None)),
    ])
    .await
    .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let error = backend
        .list(
            &OwnershipScope {
                controller_id: "controller-1".to_owned(),
                resource_name: Some("different-name".to_owned()),
            },
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        outrider::cloud::CloudError::Failure(failure) if failure.kind == "ownership"
    ));
    server.task.abort();

    let owned = instance_item_xml(
        "i-0123456789abcdef4",
        "runner-4",
        Some("controller-1"),
        Some("pool-4"),
        "subnet-123",
        "us-east-1a",
    );
    let foreign_account = instances_xml(&[owned], None).replace(
        "<item><instancesSet>",
        "<item><ownerId>foreign-account</ownerId><instancesSet>",
    );
    let server = start_aws_contract_server(vec![identity_xml(), xml_response(&foreign_account)])
        .await
        .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let error = backend
        .list(
            &OwnershipScope {
                controller_id: "controller-1".to_owned(),
                resource_name: None,
            },
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        outrider::cloud::CloudError::Failure(failure) if failure.kind == "ownership"
    ));
    server.task.abort();
}
