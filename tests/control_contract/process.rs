use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use super::daemon_scenario::{BOOTSTRAP_TOKEN, TestDirectory};
use super::provider_wire::{
    HttpFixture, bootstrap_json, provider_instance_json, provider_process, provider_response,
    write_provider_config,
};

#[test]
fn real_subprocess_all_commands() {
    let directory = TestDirectory::new().unwrap();
    let instance = provider_instance_json();
    let commands = [
        "CreateInstance",
        "DeleteInstance",
        "GetInstance",
        "ListInstances",
        "RemoveAllInstances",
        "StartInstance",
        "StopInstance",
    ];
    for command in commands {
        let result = match command {
            "CreateInstance" | "GetInstance" => Some(instance.clone()),
            "ListInstances" => Some(json!([instance.clone()])),
            _ => None,
        };

        let fixture = HttpFixture::start(
            200,
            provider_response(result.clone(), None, false).unwrap(),
            false,
        )
        .unwrap();
        let config = write_provider_config(&directory, &fixture.address, 5).unwrap();
        let body = (command == "CreateInstance").then(|| bootstrap_json().unwrap());
        let instance_id = matches!(
            command,
            "DeleteInstance" | "GetInstance" | "StartInstance" | "StopInstance"
        )
        .then_some("runner-test");
        let pool_id = matches!(command, "CreateInstance" | "ListInstances").then_some("pool-test");
        let output = provider_process(
            &config,
            command,
            None,
            instance_id,
            pool_id,
            None,
            body.as_deref(),
        )
        .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        match command {
            "CreateInstance" | "GetInstance" => {
                assert_eq!(
                    serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                    instance
                )
            }

            "ListInstances" => assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                json!([instance.clone()])
            ),
            _ => assert!(
                output.stdout.is_empty(),
                "{command} must not print a protocol value"
            ),
        }

        let request = String::from_utf8(fixture.request().unwrap()).unwrap();
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer ")
        );
        if command == "CreateInstance" {
            let body = request.split_once("\r\n\r\n").unwrap().1;
            let request_json: Value = serde_json::from_str(body).unwrap();
            assert_eq!(
                request_json["bootstrap"]["ca-cert-bundle"],
                STANDARD.encode(b"ca-secret-canary")
            );
            assert_eq!(request_json["bootstrap"]["instance-token"], BOOTSTRAP_TOKEN);
        }

        fixture.finish().unwrap();
    }

    let config = write_provider_config(&directory, "127.0.0.1:1", 5).unwrap();
    let version = provider_process(&config, "GetVersion", None, None, None, None, None).unwrap();
    assert_eq!(version.status.code(), Some(0));
    assert_eq!(version.stdout, env!("CARGO_PKG_VERSION").as_bytes());
}
