use std::{
    fs,
    process::{Command, Stdio},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use outrider::garm::{BootstrapInstance, ProviderInstance};
use serde_json::{Value, json};

use super::daemon_scenario::{INNER_TOKEN, TestDirectory};
use super::provider_wire::{
    HttpFixture, assert_schema_fields_are_present, bootstrap_json, provider_instance_json,
    provider_process, provider_response, write_provider_config,
};

#[test]
fn base64_and_stdout() {
    let input = bootstrap_json().unwrap();
    let parsed: BootstrapInstance = serde_json::from_slice(&input).unwrap();
    assert_eq!(
        parsed.ca_cert_bundle.as_deref(),
        Some(b"ca-secret-canary".as_slice())
    );
    let encoded = serde_json::to_value(parsed).unwrap();
    assert_eq!(
        encoded["ca-cert-bundle"],
        STANDARD.encode(b"ca-secret-canary")
    );
    let instance: ProviderInstance = serde_json::from_value(json!({
        "provider_id":"provider-1", "name":"runner-test", "status":"running", "provider_fault":STANDARD.encode(b"fault")
    }))
    .unwrap();
    assert_eq!(
        instance.provider_fault.as_deref(),
        Some(b"fault".as_slice())
    );
    assert_eq!(
        serde_json::to_value(instance).unwrap()["provider_fault"],
        STANDARD.encode(b"fault")
    );

    let directory = TestDirectory::new().unwrap();
    for command in ["GetConfigJSONSchema", "GetExtraSpecsJSONSchema"] {
        let config = write_provider_config(&directory, "127.0.0.1:1", 5).unwrap();
        let output =
            provider_process(&config, command, Some("v0.1.1"), None, None, None, None).unwrap();
        assert_eq!(output.status.code(), Some(0));
        let schema: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        if command == "GetExtraSpecsJSONSchema" {
            assert_eq!(schema["required"], json!(["snapshot_id"]));
        } else {
            assert_eq!(
                schema["properties"]["control"]["$ref"],
                "#/$defs/ControlSettings"
            );
            assert_schema_fields_are_present(&schema);
        }
    }

    let versions_config = write_provider_config(&directory, "127.0.0.1:1", 5).unwrap();
    let versions = provider_process(
        &versions_config,
        "GetSupportedInterfaceVersions",
        Some("v0.1.1"),
        None,
        None,
        None,
        None,
    )
    .unwrap();
    assert_eq!(versions.status.code(), Some(0));
    assert_eq!(
        serde_json::from_slice::<Value>(&versions.stdout).unwrap(),
        json!(["v0.1.0", "v0.1.1"])
    );

    let fixture = HttpFixture::start(
        503,
        provider_response(None, Some("durable pool snapshot is not registered"), false).unwrap(),
        false,
    )
    .unwrap();
    let config = write_provider_config(&directory, &fixture.address, 5).unwrap();
    let extra_specs = STANDARD.encode(br#"{"snapshot_id":"opaque-snapshot-id"}"#);
    let validation = provider_process(
        &config,
        "ValidatePoolInfo",
        Some("v0.1.1"),
        None,
        Some("pool-test"),
        Some(&extra_specs),
        None,
    )
    .unwrap();
    assert_ne!(validation.status.code(), Some(0));
    assert!(validation.stdout.is_empty());
    let request = String::from_utf8(fixture.request().unwrap()).unwrap();
    let request_body = request.split_once("\r\n\r\n").unwrap().1;
    let request_json: Value = serde_json::from_str(request_body).unwrap();
    assert_eq!(
        request_json["pool_extra_specs"]["snapshot_id"],
        "opaque-snapshot-id"
    );
    fixture.finish().unwrap();
}

#[test]
fn provider_normalizes_empty_interface_and_rejects_invalid_config() {
    let directory = TestDirectory::new().unwrap();
    let expected = provider_instance_json();
    let fixture = HttpFixture::start(
        200,
        provider_response(Some(expected.clone()), None, false).unwrap(),
        false,
    )
    .unwrap();
    let config = write_provider_config(&directory, &fixture.address, 5).unwrap();
    let output = provider_process(
        &config,
        "GetInstance",
        Some(""),
        Some("runner-test"),
        Some("pool-test"),
        None,
        None,
    )
    .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        expected
    );
    let request = String::from_utf8(fixture.request().unwrap()).unwrap();
    let request_body = request.split_once("\r\n\r\n").unwrap().1;
    assert_eq!(
        serde_json::from_str::<Value>(request_body).unwrap()["interface_version"],
        "v0.1.0"
    );
    fixture.finish().unwrap();

    let config = write_provider_config(&directory, "127.0.0.1:1", 0).unwrap();
    let timeout = provider_process(
        &config,
        "GetInstance",
        None,
        Some("runner-test"),
        Some("pool-test"),
        None,
        None,
    )
    .unwrap();
    assert_ne!(timeout.status.code(), Some(0));
    assert!(timeout.stdout.is_empty());
    assert!(String::from_utf8_lossy(&timeout.stderr).contains("provider_timeout_seconds"));

    let config = write_provider_config(&directory, "127.0.0.1:1", 5).unwrap();
    fs::write(directory.path().join("token"), "  \n").unwrap();
    let empty_token = provider_process(
        &config,
        "GetInstance",
        None,
        Some("runner-test"),
        Some("pool-test"),
        None,
        None,
    )
    .unwrap();
    assert_ne!(empty_token.status.code(), Some(0));
    assert!(empty_token.stdout.is_empty());
    assert!(String::from_utf8_lossy(&empty_token.stderr).contains("auth token file is empty"));

    let empty_controller = Command::new(env!("CARGO_BIN_EXE_outrider"))
        .env("GARM_COMMAND", "GetVersion")
        .env("GARM_CONTROLLER_ID", "  ")
        .env("GARM_PROVIDER_CONFIG_FILE", &config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = empty_controller.wait_with_output().unwrap();
    assert_ne!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("GARM_CONTROLLER_ID is required"));
}

#[test]
fn create_requires_a_valid_ingress_timeout_budget_before_connecting() {
    let directory = TestDirectory::new().unwrap();
    let body = bootstrap_json().unwrap();

    let config = write_provider_config(&directory, "127.0.0.1:1", 1).unwrap();
    let complete = fs::read_to_string(&config).unwrap();
    let control_only = complete.split_once("\n[ingress]").unwrap().0;
    fs::write(&config, control_only).unwrap();
    let missing = provider_process(
        &config,
        "CreateInstance",
        None,
        None,
        Some("pool-test"),
        None,
        Some(&body),
    )
    .unwrap();
    assert_ne!(missing.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .contains("ingress settings are required for CreateInstance")
    );

    let config = write_provider_config(&directory, "127.0.0.1:1", 1).unwrap();
    let invalid_margin = fs::read_to_string(&config).unwrap().replace(
        "timeout_margin_seconds = 30",
        "timeout_margin_seconds = 600",
    );
    fs::write(&config, invalid_margin).unwrap();
    let invalid = provider_process(
        &config,
        "CreateInstance",
        None,
        None,
        Some("pool-test"),
        None,
        Some(&body),
    )
    .unwrap();
    assert_ne!(invalid.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&invalid.stderr)
            .contains("invalid ingress CreateInstance timeout budget")
    );

    let config = write_provider_config(&directory, "127.0.0.1:1", 1).unwrap();
    let overflow = fs::read_to_string(&config).unwrap().replace(
        "bootstrap_timeout_minutes = 20",
        "bootstrap_timeout_minutes = 4294967296",
    );
    fs::write(&config, overflow).unwrap();
    let overflow = provider_process(
        &config,
        "CreateInstance",
        None,
        None,
        Some("pool-test"),
        None,
        Some(&body),
    )
    .unwrap();
    assert_ne!(overflow.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&overflow.stderr).contains("invalid provider configuration"));
}

#[test]
fn daemon_connection_errors_are_classified_without_request_details() {
    let directory = TestDirectory::new().unwrap();
    let config = write_provider_config(&directory, "127.0.0.1:1", 1).unwrap();
    let document = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        format!("{document}\n[garm]\nconfig_file = '/missing/native-garm.toml'\n"),
    )
    .unwrap();

    let output = provider_process(
        &config,
        "GetInstance",
        None,
        Some("runner-test"),
        Some("pool-test"),
        None,
        None,
    )
    .unwrap();
    assert_ne!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "outrider provider: cannot connect to daemon"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("127.0.0.1"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(INNER_TOKEN));
}
