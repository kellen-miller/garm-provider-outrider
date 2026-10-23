use super::cloud_server::{
    AWS_CHILD_CONFIG, RegionalPublicIpv4Quote, aws_backend, aws_backend_with_settings,
    aws_settings, candidate, compute_product, disk_products, identity_xml, image_xml,
    image_xml_with_root_volume_size, instance_type_xml, instance_type_xml_with_throughput,
    ipv4_product, offering_xml, pricing_response, regional_public_ipv4_request_matches,
    resolved_profile, start_aws_contract_server, subnet_xml, xml_response,
};
use std::{collections::BTreeSet, fs, time::Duration};

use outrider::{
    cloud::{CatalogQuery, CloudCatalog, aws::AwsLocation},
    policy::ProviderLaunchFacts,
};

use rust_decimal::Decimal;
use serde_json::json;
use sha2::{Digest, Sha256};

#[tokio::test]
async fn provider_launch_defaults_and_quote() {
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
    let offer = candidates.first().unwrap();
    let selection = &offer.selection;
    assert_eq!(selection.binding_id, "aws-main");
    assert_eq!(selection.flavor, "m7i.xlarge");
    assert_eq!(selection.image, "ami-123");
    let ProviderLaunchFacts::Aws(facts) = &offer.launch_facts else {
        panic!("expected AWS launch facts")
    };
    assert_eq!(facts.subnet_id, "subnet-123");
    assert!(facts.map_public_ip_on_launch);
    assert_eq!(selection.approved_extras["subnet_id"], json!("subnet-123"));
    assert_eq!(
        selection.approved_extras["security_group_ids"],
        json!(["sg-123"])
    );
    assert_eq!(selection.approved_extras["spot"], json!(false));
    assert_eq!(selection.approved_extras["volume_type"], json!("gp3"));
    assert_eq!(selection.approved_extras["volume_size"], json!(40));
    assert_eq!(selection.approved_extras["iops"], json!(4000));
    assert_eq!(selection.approved_extras["throughput"], json!(225));
    let quote = backend
        .quote(
            offer,
            Duration::from_secs(3600),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(quote.provider_request_ids, ["pricing-request"; 3]);
    assert_eq!(quote.compute.hourly_rate, Some(Decimal::new(10, 2)));
    assert_eq!(
        quote.disk.hourly_rate,
        Some(Decimal::from_str_exact("0.0240").unwrap())
    );
    let requests = server.requests.lock().unwrap();
    assert!(requests.iter().all(|request| {
        [
            "RunInstances",
            "StartInstances",
            "StopInstances",
            "TerminateInstances",
            "DeleteVolume",
        ]
        .iter()
        .all(|action| !request.body.contains(action))
    }));
    server.task.abort();
}

#[tokio::test]
async fn catalog_reports_requested_compute_exclusion() {
    let server = start_aws_contract_server(vec![
        identity_xml(),
        xml_response(image_xml()),
        xml_response(subnet_xml()),
        xml_response(instance_type_xml()),
        xml_response("<DescribeInstanceTypesResponse/>"),
        xml_response(offering_xml()),
        xml_response("<DescribeInstanceTypeOfferingsResponse/>"),
    ])
    .await
    .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let mut profile = resolved_profile().unwrap().profile;
    profile.vcpu_min = 16;
    profile.vcpu_max = 32;
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
    assert!(discovery.queried);
    assert!(discovery.offers.is_empty());
    assert!(discovery.exclusions.iter().any(|exclusion| {
        exclusion.reason == "machine type does not meet requested architecture or compute bounds"
            && exclusion.observed_count == 1
    }));
    server.task.abort();
}

async fn regional_public_ipv4_quote(
    scenario: &str,
) -> Result<RegionalPublicIpv4Quote, Box<dyn std::error::Error>> {
    let server = start_aws_contract_server(vec![
        identity_xml(),
        xml_response(&subnet_xml().replace("us-east-1a", "eu-west-1a")),
        xml_response(image_xml()),
        pricing_response(Vec::new()),
        pricing_response(Vec::new()),
        xml_response(&format!("regional-public-ipv4:{scenario}")),
    ])
    .await?;
    let mut settings = aws_settings(&server.endpoint);
    let child_config = AWS_CHILD_CONFIG.replace("us-east-1", "eu-west-1");
    fs::write(&settings.provider_configuration_path, &child_config)?;
    settings.configuration_sha256 = format!("{:x}", Sha256::digest(child_config.as_bytes()));
    settings.region = "eu-west-1".to_owned();
    settings.pricing_location = "Europe (Ireland)".to_owned();
    settings.locations.clear();
    settings.locations.insert(
        "eu-west-1".to_owned(),
        AwsLocation {
            region: "eu-west-1".to_owned(),
            availability_zone: "eu-west-1a".to_owned(),
            subnet_id: "subnet-123".to_owned(),
            security_group_ids: vec!["sg-123".to_owned()],
        },
    );
    let backend = aws_backend_with_settings(&server.endpoint, settings.clone()).await?;
    let mut offer = candidate("ondemand");
    offer.candidate.location = "eu-west-1".to_owned();
    offer.candidate.identity = offer.candidate.identity.replace("us-east-1", "eu-west-1");
    offer.selection.configuration_sha256 = settings.configuration_sha256;
    offer
        .selection
        .approved_extras
        .insert("subnet_id".to_owned(), json!("subnet-123"));
    offer
        .selection
        .approved_extras
        .insert("security_group_ids".to_owned(), json!(["sg-123"]));
    let ProviderLaunchFacts::Aws(facts) = &mut offer.launch_facts else {
        panic!("expected AWS launch facts")
    };
    facts.availability_zone = "eu-west-1a".to_owned();
    facts.root_volume_minimum_gib = 32;
    let quote = backend
        .quote(
            &offer,
            Duration::from_secs(3600),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await?;
    let requests = server
        .requests
        .lock()
        .map_err(|_| std::io::Error::other("AWS request lock is poisoned"))?;
    let request_body = requests
        .last()
        .ok_or("regional public IPv4 request was not captured")?
        .body
        .clone();
    server.task.abort();
    Ok(RegionalPublicIpv4Quote {
        hourly_rate: quote.public_address.hourly_rate,
        request_body,
    })
}

#[tokio::test]
async fn regional_public_ipv4_price_uses_region_prefixed_usage_type()
-> Result<(), Box<dyn std::error::Error>> {
    let quote = regional_public_ipv4_quote("valid").await?;
    assert_eq!(quote.hourly_rate, Some(Decimal::new(5, 3)));
    assert!(regional_public_ipv4_request_matches(&quote.request_body));
    Ok(())
}

#[tokio::test]
async fn regional_public_ipv4_excludes_wrong_region_and_idle_products()
-> Result<(), Box<dyn std::error::Error>> {
    let wrong_region = regional_public_ipv4_quote("wrong-region").await?;
    assert_eq!(wrong_region.hourly_rate, None);

    let idle = regional_public_ipv4_quote("idle").await?;
    assert_eq!(idle.hourly_rate, None);
    Ok(())
}

#[tokio::test]
async fn regional_public_ipv4_ambiguous_products_remain_unknown()
-> Result<(), Box<dyn std::error::Error>> {
    let quote = regional_public_ipv4_quote("ambiguous").await?;
    assert_eq!(quote.hourly_rate, None);
    Ok(())
}

#[tokio::test]
async fn premium_and_sql_images_have_unknown_generic_license_price() {
    for (os, platform, usage_operation) in [
        ("linux", "Red Hat Enterprise Linux", "RunInstances:0010"),
        (
            "windows",
            "Windows with SQL Server Standard",
            "RunInstances:0006",
        ),
    ] {
        let image_response = format!(
            "<DescribeImagesResponse><imagesSet><item><imageId>ami-123</imageId><imageState>available</imageState><architecture>x86_64</architecture><platformDetails>{platform}</platformDetails><usageOperation>{usage_operation}</usageOperation><rootDeviceName>/dev/xvda</rootDeviceName><blockDeviceMapping><item><deviceName>/dev/xvda</deviceName><ebs><snapshotId>snap-123</snapshotId><deleteOnTermination>true</deleteOnTermination></ebs></item></blockDeviceMapping></item></imagesSet></DescribeImagesResponse>"
        );
        let server = start_aws_contract_server(vec![
            identity_xml(),
            xml_response(subnet_xml()),
            xml_response(&image_response),
        ])
        .await
        .unwrap();
        let mut settings = aws_settings(&server.endpoint);
        let image = settings.images.get_mut("ubuntu-24").unwrap();
        image.os = os.to_owned();
        image.windows_license_included = os == "windows";
        let backend = aws_backend_with_settings(&server.endpoint, settings)
            .await
            .unwrap();
        let mut offer = candidate("on_demand");
        offer.candidate.os = os.to_owned();
        let result = backend
            .quote(
                &offer,
                Duration::from_secs(3600),
                Duration::from_secs(600),
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await;
        assert!(
            result.is_err(),
            "unpriced premium image launched a generic license quote"
        );
        assert_eq!(server.requests.lock().unwrap().len(), 3);
        server.task.abort();
    }
}

#[tokio::test]
async fn fractional_disk_throughput() {
    let server = start_aws_contract_server(vec![identity_xml()])
        .await
        .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let mut offer = candidate("on_demand");
    offer.candidate.boot_disk_throughput_mib_s = Decimal::from_str_exact("125.5").unwrap();
    offer
        .selection
        .approved_extras
        .insert("throughput".to_owned(), json!(125.5));
    let error = backend
        .quote(
            &offer,
            Duration::from_secs(3600),
            Duration::from_secs(600),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        outrider::cloud::CloudError::Failure(failure) if failure.kind == "unsupported"
    ));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    server.task.abort();
}

#[tokio::test]
async fn launch_disk_feasibility() {
    for maximum_throughput_mbps in [Some("235.405312"), None] {
        let server = start_aws_contract_server(vec![
            identity_xml(),
            xml_response(image_xml()),
            xml_response(subnet_xml()),
            xml_response(&instance_type_xml_with_throughput(maximum_throughput_mbps, "default", Some(5000))),
            xml_response("<DescribeInstanceTypeOfferingsResponse><instanceTypeOfferingSet><item><instanceType>m7i.xlarge</instanceType><location>us-east-1a</location><locationType>availability-zone</locationType></item></instanceTypeOfferingSet></DescribeInstanceTypeOfferingsResponse>"),
        ])
        .await
        .unwrap();
        let backend = aws_backend(&server.endpoint).await.unwrap();
        let mut profile = resolved_profile().unwrap().profile;
        profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
        let offers = backend
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
        assert!(offers.is_empty());
        server.task.abort();
    }

    let server = start_aws_contract_server(vec![
        identity_xml(),
        xml_response(image_xml()),
        xml_response(subnet_xml()),
        xml_response(&instance_type_xml_with_throughput(Some("236.453888"), "default", Some(5000))),
        xml_response("<DescribeInstanceTypeOfferingsResponse><instanceTypeOfferingSet><item><instanceType>m7i.xlarge</instanceType><location>us-east-1a</location><locationType>availability-zone</locationType></item></instanceTypeOfferingSet></DescribeInstanceTypeOfferingsResponse>"),
    ])
    .await
    .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let mut profile = resolved_profile().unwrap().profile;
    profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    profile.administrator_limits.max_boot_disk_throughput_mib_s = 225;
    let offers = backend
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
    assert_eq!(offers.len(), 1);
    assert_eq!(
        offers[0].candidate.boot_disk_throughput_mib_s,
        Decimal::from(225u32)
    );
    let ProviderLaunchFacts::Aws(facts) = &offers[0].launch_facts else {
        panic!("expected AWS launch facts")
    };
    assert_eq!(
        facts.maximum_ebs_throughput_mib_s,
        Decimal::from_str_exact("225.5").unwrap()
    );
    assert_eq!(
        offers[0].selection.approved_extras["throughput"],
        json!(225)
    );
    server.task.abort();

    let server = start_aws_contract_server(vec![
        identity_xml(),
        xml_response(image_xml()),
        xml_response(subnet_xml()),
        xml_response(&instance_type_xml_with_throughput(Some("1000"), "supported", Some(5000))),
        xml_response("<DescribeInstanceTypeOfferingsResponse><instanceTypeOfferingSet><item><instanceType>m7i.xlarge</instanceType><location>us-east-1a</location><locationType>availability-zone</locationType></item></instanceTypeOfferingSet></DescribeInstanceTypeOfferingsResponse>"),
    ])
    .await
    .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let mut profile = resolved_profile().unwrap().profile;
    profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    let offers = backend
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
    assert!(offers.is_empty());
    server.task.abort();

    let server = start_aws_contract_server(vec![
        identity_xml(),
        xml_response(image_xml()),
        xml_response(subnet_xml()),
        xml_response(&instance_type_xml_with_throughput(Some("1000"), "default", Some(3000))),
        xml_response("<DescribeInstanceTypeOfferingsResponse><instanceTypeOfferingSet><item><instanceType>m7i.xlarge</instanceType><location>us-east-1a</location><locationType>availability-zone</locationType></item></instanceTypeOfferingSet></DescribeInstanceTypeOfferingsResponse>"),
    ])
    .await
    .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let mut profile = resolved_profile().unwrap().profile;
    profile.purchasing_models = BTreeSet::from(["on_demand".to_owned()]);
    profile.boot_disk.iops = Some(4000);
    let offers = backend
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
    assert!(offers.is_empty());
    server.task.abort();

    let server = start_aws_contract_server(vec![
        identity_xml(),
        xml_response(&image_xml_with_root_volume_size(64)),
    ])
    .await
    .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let error = backend
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
        .unwrap_err();
    assert!(matches!(
        error,
        outrider::cloud::CloudError::Failure(failure) if failure.kind == "unsupported"
    ));
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    server.task.abort();

    let image_without_root_size = image_xml().replace("<volumeSize>32</volumeSize>", "");
    let server =
        start_aws_contract_server(vec![identity_xml(), xml_response(&image_without_root_size)])
            .await
            .unwrap();
    let backend = aws_backend(&server.endpoint).await.unwrap();
    let error = backend
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
        .unwrap_err();
    assert!(matches!(
        error,
        outrider::cloud::CloudError::Failure(failure) if failure.kind == "configuration"
    ));
    server.task.abort();
}
