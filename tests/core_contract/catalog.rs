use super::config::central_patch;
use std::collections::BTreeMap;
use std::io;

use outrider::{config::ConfigPatch, label::RoutingRequest};

#[test]
fn effective_prices_are_strict_central_only_and_non_overlapping() {
    let scope = r#"
      cloud: aws
      configured_account: build-account
      native_account: "123456789012"
      configured_location: east-ci
      native_location: us-east-1
      binding_id: aws-east
      machine_type: c7i.large
      os: linux
      architecture: amd64
      image_alias: ubuntu-24
      resolved_image: ami-0123456789abcdef0
      purchasing: on_demand
      vcpu: 2
      memory_gib: 4
      gpu_count: 0
      boot_disk_class: gp3
      boot_disk_gib: 30
      boot_disk_iops: 3000
      boot_disk_throughput_mib_s: 125
      local_scratch_gib: 0
      launch_facts:
        cloud: aws
        facts:
          availability_zone: us-east-1a
          subnet_id: subnet-123
          security_group_ids: []
          map_public_ip_on_launch: false
          resolved_image_id: ami-0123456789abcdef0
          windows_license_included: false
          root_device_name: /dev/sda1
          minimum_root_volume_gib: 30
"#;
    let reported_scope: outrider::allocation::FinancialPriceScope =
        serde_yaml_ng::from_str(scope).unwrap();
    let report_json = serde_json::to_value(&reported_scope).unwrap();
    let copied_scope = serde_yaml_ng::to_string(&report_json)
        .unwrap()
        .lines()
        .map(|line| format!("      {line}\n"))
        .collect::<String>();
    let record = |id: &str, component: &str, from: u64, until: u64| {
        format!(
            "  - id: \"{id}\"\n    scope:\n{copied_scope}    component: {component}\n    currency: USD\n    hourly_rate: \"0.42\"\n    source: vendor agreement\n    reference: agreement-section-4\n    observed_at_unix_seconds: 1792600000\n    valid_from_unix_seconds: {from}\n    valid_until_unix_seconds: {until}\n",
        )
    };
    let valid_yaml = format!(
        "version: 1\neffective_prices:\n{}",
        record("price-compute", "compute", 1792800000, 1800480000)
    );
    let central = ConfigPatch::from_yaml(&valid_yaml).unwrap();
    assert!(central.effective_prices.is_some());
    assert_effective_price_schema_matches(&report_json, &valid_yaml);
    assert_effective_price_decimal_contract(&valid_yaml, &report_json);
    let blank_source = valid_yaml.replace("source: vendor agreement", "source: '   '");
    assert!(ConfigPatch::from_yaml(&blank_source).is_err());
    assert_effective_price_schema_rejects(&blank_source);
    let controlled_reference = valid_yaml.replace(
        "reference: agreement-section-4",
        "reference: \"bad\\u0001value\"",
    );
    assert!(ConfigPatch::from_yaml(&controlled_reference).is_err());
    assert_effective_price_schema_rejects(&controlled_reference);
    let repository = ConfigPatch::from_yaml(&valid_yaml).unwrap();
    assert!(matches!(
        central.normalize_profile(Some(&repository), "build", None),
        Err(outrider::config::ConfigError::ForbiddenOverride(_))
    ));

    let overlap = format!(
        "version: 1\neffective_prices:\n{}{}",
        record("price-compute-a", "compute", 1792800000, 1800480000),
        record("price-compute-b", "compute", 1800000000, 1805000000),
    );
    assert!(ConfigPatch::from_yaml(&overlap).is_err());
    let adjacent = format!(
        "version: 1\neffective_prices:\n{}{}",
        record("price-compute-a", "compute", 1792800000, 1800480000),
        record("price-compute-b", "compute", 1800480000, 1805000000),
    );
    assert!(ConfigPatch::from_yaml(&adjacent).is_ok());
    let different_components_overlap = format!(
        "version: 1\neffective_prices:\n{}{}",
        record("price-compute", "compute", 1792800000, 1800480000),
        record("price-disk", "disk", 1792800000, 1800480000),
    );
    assert!(ConfigPatch::from_yaml(&different_components_overlap).is_ok());
    let duplicate_id = format!(
        "version: 1\neffective_prices:\n{}{}",
        record("duplicate-id", "compute", 1792800000, 1800480000),
        record("duplicate-id", "disk", 1792800000, 1800480000),
    );
    assert!(ConfigPatch::from_yaml(&duplicate_id).is_err());
    let padded_id = format!(
        "version: 1\neffective_prices:\n{}",
        record(" padded-id", "compute", 1792800000, 1800480000),
    );
    assert!(ConfigPatch::from_yaml(&padded_id).is_err());
    let punctuation_id = format!(
        "version: 1\neffective_prices:\n{}",
        record("-leading-id", "compute", 1792800000, 1800480000),
    );
    assert!(ConfigPatch::from_yaml(&punctuation_id).is_err());
    assert_effective_price_schema_rejects(&punctuation_id);
    let oversized_id = format!(
        "version: 1\neffective_prices:\n{}",
        record(&"x".repeat(129), "compute", 1792800000, 1800480000),
    );
    assert!(ConfigPatch::from_yaml(&oversized_id).is_err());
    let unquoted_rate = valid_yaml.replace("\"0.42\"", "0.42");
    assert!(ConfigPatch::from_yaml(&unquoted_rate).is_err());
    let mismatched_launch_facts = valid_yaml.replace("cloud: aws", "cloud: azure");
    assert!(ConfigPatch::from_yaml(&mismatched_launch_facts).is_err());
    assert!(
        ConfigPatch::from_yaml(&valid_yaml.replace("component: compute", "component: network"))
            .is_err()
    );
}

fn assert_effective_price_decimal_contract(valid_yaml: &str, report_json: &serde_json::Value) {
    for rate in [
        "6.1234567890123456789012345678",
        "69.123456789012345678901234567",
        "6000000000000000000000000000.1",
    ] {
        let exact_rate = valid_yaml.replace("\"0.42\"", &format!("\"{rate}\""));
        assert!(ConfigPatch::from_yaml(&exact_rate).is_ok());
        assert_effective_price_schema_matches(report_json, &exact_rate);
    }

    let exact_scale_rate = valid_yaml.replace("\"0.42\"", "\"0.0000000000000000000000000001\"");
    assert!(ConfigPatch::from_yaml(&exact_scale_rate).is_ok());
    assert_effective_price_schema_matches(report_json, &exact_scale_rate);
    let maximum_decimal_rate = valid_yaml.replace("\"0.42\"", "\"79228162514264337593543950334\"");
    assert!(ConfigPatch::from_yaml(&maximum_decimal_rate).is_ok());
    assert_effective_price_schema_matches(report_json, &maximum_decimal_rate);
    let maximum_fractional_rate =
        valid_yaml.replace("\"0.42\"", "\"7.9228162514264337593543950335\"");
    assert!(ConfigPatch::from_yaml(&maximum_fractional_rate).is_ok());
    assert_effective_price_schema_matches(report_json, &maximum_fractional_rate);
    let fractional_maximum_plus_one =
        valid_yaml.replace("\"0.42\"", "\"7.9228162514264337593543950336\"");
    assert!(ConfigPatch::from_yaml(&fractional_maximum_plus_one).is_err());
    assert_effective_price_schema_rejects(&fractional_maximum_plus_one);
    let overprecision_rate = valid_yaml.replace("\"0.42\"", "\"0.00000000000000000000000000001\"");
    assert!(ConfigPatch::from_yaml(&overprecision_rate).is_err());
    assert_effective_price_schema_rejects(&overprecision_rate);
    let overflow_rate = valid_yaml.replace("\"0.42\"", "\"79228162514264337593543950336\"");
    assert!(ConfigPatch::from_yaml(&overflow_rate).is_err());
    assert_effective_price_schema_rejects(&overflow_rate);
    let fractional_overflow = valid_yaml.replace("\"0.42\"", "\"9.9999999999999999999999999999\"");
    assert!(ConfigPatch::from_yaml(&fractional_overflow).is_err());
    assert_effective_price_schema_rejects(&fractional_overflow);
}

fn assert_effective_price_schema_rejects(central_config_yaml: &str) {
    let schema = outrider::config::json_schema();
    let Ok(validator) = jsonschema::validator_for(&schema) else {
        panic!("effective price config schema did not compile");
    };
    let Ok(yaml_value) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(central_config_yaml)
    else {
        panic!("invalid-rate test configuration was not valid YAML");
    };
    let Ok(config_json) = serde_json::to_value(yaml_value) else {
        panic!("invalid-rate test configuration was not JSON representable");
    };
    assert!(!validator.is_valid(&config_json));
}

fn assert_effective_price_schema_matches(
    report_json: &serde_json::Value,
    central_config_yaml: &str,
) {
    let schema = outrider::config::json_schema();
    assert_eq!(
        schema["properties"]["effective_prices"]["items"]["$ref"],
        "#/$defs/EffectivePrice"
    );
    assert_eq!(
        schema["$defs"]["EffectivePrice"]["additionalProperties"],
        false
    );
    assert_eq!(
        schema["$defs"]["FinancialPriceScope"]["additionalProperties"],
        false
    );
    assert_eq!(
        schema["$defs"]["EffectivePrice"]["properties"]["scope"]["$ref"],
        "#/$defs/FinancialPriceScope"
    );
    assert_eq!(
        schema["$defs"]["FinancialPriceScope"]["properties"]["launch_facts"]["$ref"],
        "#/$defs/FinancialLaunchFacts"
    );
    assert_eq!(
        schema["$defs"]["EffectivePrice"]["properties"]["hourly_rate"]["type"],
        "string"
    );
    assert_eq!(
        schema["$defs"]["EffectivePrice"]["properties"]["component"]["enum"],
        serde_json::json!(["compute", "disk", "licensing", "public_address"])
    );
    assert!(
        schema["$defs"]["FinancialPriceScope"]["required"]
            .as_array()
            .is_some_and(|required| required.iter().any(|field| field == "launch_facts"))
    );
    assert!(
        schema["$defs"]["FinancialLaunchFacts"]["oneOf"]
            .as_array()
            .is_some_and(|variants| variants.len() == 3)
    );
    let Some(scope_properties) = schema["$defs"]["FinancialPriceScope"]["properties"].as_object()
    else {
        panic!("financial scope schema omitted properties");
    };
    let Some(report_scope_fields) = report_json.as_object() else {
        panic!("serialized financial scope was not an object");
    };
    assert_eq!(scope_properties.len(), 24);
    assert!(
        report_scope_fields
            .keys()
            .all(|field| scope_properties.contains_key(field))
    );
    let Some(required_scope_fields) = schema["$defs"]["FinancialPriceScope"]["required"].as_array()
    else {
        panic!("financial scope schema omitted its required fields");
    };
    assert!(required_scope_fields.iter().all(|field| {
        field
            .as_str()
            .is_some_and(|field| report_scope_fields.contains_key(field))
    }));
    let Some(aws_launch_schema) = schema["$defs"]["FinancialLaunchFacts"]["oneOf"][0]["properties"]
        ["facts"]["properties"]
        .as_object()
    else {
        panic!("AWS launch schema omitted properties");
    };
    let Some(aws_launch_facts) = report_json["launch_facts"]["facts"].as_object() else {
        panic!("serialized AWS launch facts were not an object");
    };
    assert!(
        aws_launch_facts
            .keys()
            .all(|field| aws_launch_schema.contains_key(field))
    );
    let Ok(validator) = jsonschema::validator_for(&schema) else {
        panic!("effective price config schema did not compile");
    };
    let Ok(yaml_value) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(central_config_yaml)
    else {
        panic!("Explain scope central config was not valid YAML");
    };
    let Ok(config_json) = serde_json::to_value(yaml_value) else {
        panic!("Explain scope central config was not JSON representable");
    };
    assert!(validator.is_valid(&config_json));

    let mut partial_scope = config_json.clone();
    let Some(partial_scope_fields) = partial_scope["effective_prices"][0]["scope"].as_object_mut()
    else {
        panic!("Explain scope config omitted a scope object");
    };
    partial_scope_fields.remove("native_location");
    assert!(!validator.is_valid(&partial_scope));

    let mut unknown_scope = config_json;
    let Some(unknown_scope_fields) = unknown_scope["effective_prices"][0]["scope"].as_object_mut()
    else {
        panic!("Explain scope config omitted a scope object");
    };
    unknown_scope_fields.insert("wildcard".to_owned(), serde_json::json!("*"));
    assert!(!validator.is_valid(&unknown_scope));
}

#[test]
fn azure_catalog_binding_is_central_only() {
    let central = central_patch().unwrap();
    let mut repository = ConfigPatch::from_yaml("version: 1\nprofiles:\n  build: {}\n").unwrap();
    repository.azure_catalog_bindings = Some(Vec::new());

    assert!(matches!(
        central.normalize_profile(Some(&repository), "build", None),
        Err(outrider::config::ConfigError::ForbiddenOverride(_))
    ));

    let schema = outrider::config::json_schema();
    assert_eq!(
        schema["properties"]["azure_catalog_bindings"]["items"]["$ref"],
        "#/$defs/AzureCatalogBinding"
    );
    assert!(
        schema["$defs"]["AzureCatalogBinding"]["required"]
            .as_array()
            .is_some_and(|required| required.iter().any(|field| field == "accelerator_facts"))
    );
    assert_eq!(
        schema["$defs"]["AzureCatalogBinding"]["properties"]["accelerator_facts"]["additionalProperties"]
            ["$ref"],
        "#/$defs/AzureAcceleratorFacts"
    );
    assert!(
        schema["$defs"]["AzureImageBinding"]["required"]
            .as_array()
            .is_some_and(|required| {
                required.iter().any(|field| field == "licensing")
                    && required.iter().any(|field| field == "licensing_source")
                    && !required
                        .iter()
                        .any(|field| field == "windows_license_included")
            })
    );
    assert_eq!(
        schema["$defs"]["AzureImageBinding"]["properties"]["licensing"]["enum"][0],
        "no_additional_software_charge"
    );
}

#[test]
fn gcp_catalog_binding_is_central_only() {
    let central = central_patch().unwrap();
    let mut repository = ConfigPatch::from_yaml("version: 1\nprofiles:\n  build: {}\n").unwrap();
    repository.gcp_catalog_bindings = Some(Vec::new());

    assert!(matches!(
        central.normalize_profile(Some(&repository), "build", None),
        Err(outrider::config::ConfigError::ForbiddenOverride(_))
    ));

    let schema = outrider::config::json_schema();
    assert_eq!(
        schema["properties"]["gcp_catalog_bindings"]["items"]["$ref"],
        "#/$defs/GcpCatalogBinding"
    );
    assert_eq!(
        schema["$defs"]["GcpCatalogBinding"]["properties"]["capacity_advice_enabled"]["default"],
        false
    );
}

#[test]
fn canonical_token_reject_variants() {
    let token = "outrider:v1:123:456:1:build:image=Ubuntu%2d24:vcpu=8";
    assert!(RoutingRequest::parse(token).is_err());
    assert!(RoutingRequest::parse("outrider:v1:123:456:1:build:vcpu=8:image=ubuntu-24").is_err());
    assert!(RoutingRequest::parse("outrider:v1:123:456:1:build:vcpu=8:vcpu=9").is_err());
    assert!(RoutingRequest::parse("outrider:v1:123:456:1:BUILD:vcpu=8").is_err());
    assert!(RoutingRequest::parse("outrider:v1:123:456:1:build:image=ubuntu%2D24").is_err());
}

#[test]
fn distinct_override_tokens() {
    let default = RoutingRequest {
        repository_id: 123,
        run_id: 456,
        run_attempt: 1,
        profile: "build".to_owned(),
        overrides: BTreeMap::new(),
    }
    .format()
    .unwrap();
    let mut overrides = BTreeMap::new();
    overrides.insert("vcpu".to_owned(), "8".to_owned());
    let changed = RoutingRequest {
        repository_id: 123,
        run_id: 456,
        run_attempt: 1,
        profile: "build".to_owned(),
        overrides,
    }
    .format()
    .unwrap();
    assert_ne!(default, changed);
    assert_eq!(
        RoutingRequest::parse(&changed).unwrap().format().unwrap(),
        changed
    );
    assert_patch_override_round_trip_and_validation().unwrap();
    assert!(
        RoutingRequest {
            repository_id: 123,
            run_id: 456,
            run_attempt: 1,
            profile: "build".to_owned(),
            overrides: [("memory".to_owned(), "016gib".to_owned())].into(),
        }
        .format()
        .is_err()
    );
}

fn assert_patch_override_round_trip_and_validation() -> Result<(), Box<dyn std::error::Error>> {
    let patch_token = RoutingRequest {
        repository_id: 123,
        run_id: 456,
        run_attempt: 1,
        profile: "build".to_owned(),
        overrides: [(
            "patch".to_owned(),
            r#"{"boot_disk":{"size_gib":80},"gpu":{"count":1,"vendor":"nvidia"}}"#.to_owned(),
        )]
        .into(),
    }
    .format()?;
    let decoded = RoutingRequest::parse(&patch_token)?.job_overrides()?;
    let boot_disk = decoded
        .profile
        .boot_disk
        .ok_or_else(|| io::Error::other("boot disk patch is missing"))?;
    let gpu = decoded
        .profile
        .gpu
        .ok_or_else(|| io::Error::other("GPU patch is missing"))?;
    assert_eq!(boot_disk.size_gib, Some(80));
    assert_eq!(gpu.vendor.as_deref(), Some("nvidia"));
    for patch in [
        r#"{"image":"a","image":"b"}"#,
        r#"{"boot_disk":{"size_gib":20,"size_gib":30}}"#,
        r#"{"unknown":"value"}"#,
        r#"{"image":null}"#,
        r#"{ "image":"ubuntu-24"}"#,
    ] {
        assert!(
            RoutingRequest {
                repository_id: 123,
                run_id: 456,
                run_attempt: 1,
                profile: "build".to_owned(),
                overrides: [("patch".to_owned(), patch.to_owned())].into(),
            }
            .format()
            .is_err()
        );
    }

    assert!(
        RoutingRequest::parse(
            "outrider:v1:123:456:1:build:patch=%7b%22image%22%3a%22a%22%2c%22image%22%3a%22b%22%7d"
        )
        .is_err()
    );
    let conflicting = RoutingRequest {
        repository_id: 123,
        run_id: 456,
        run_attempt: 1,
        profile: "build".to_owned(),
        overrides: [
            ("image".to_owned(), "ubuntu-24".to_owned()),
            ("patch".to_owned(), r#"{"image":"ubuntu-24"}"#.to_owned()),
        ]
        .into(),
    };

    assert!(conflicting.job_overrides().is_err());
    Ok(())
}
