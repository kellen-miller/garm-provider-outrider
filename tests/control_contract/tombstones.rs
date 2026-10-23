use std::{
    io::Write,
    process::{Command, Stdio},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::Connection;
use serde_json::{Value, json};

use super::daemon_scenario::{BOOTSTRAP_TOKEN, TestDaemon, TestDirectory};
use super::provider_wire::{
    HttpFixture, bootstrap_json, provider_process, provider_response, write_provider_config,
};

#[test]
fn provider_disconnect_delete_tombstone() {
    let directory = TestDirectory::new().unwrap();
    let fixture =
        HttpFixture::start(200, provider_response(None, None, false).unwrap(), true).unwrap();
    let config = write_provider_config(&directory, &fixture.address, 15).unwrap();
    let mut provider = Command::new(env!("CARGO_BIN_EXE_outrider"))
        .env("GARM_COMMAND", "CreateInstance")
        .env("GARM_CONTROLLER_ID", "controller-test")
        .env("GARM_POOL_ID", "pool-test")
        .env("GARM_PROVIDER_CONFIG_FILE", &config)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    provider
        .stdin
        .take()
        .unwrap()
        .write_all(&bootstrap_json().unwrap())
        .unwrap();
    let request = String::from_utf8(fixture.request().unwrap()).unwrap();
    assert!(request.contains(BOOTSTRAP_TOKEN));
    assert!(provider.try_wait().unwrap().is_none());
    provider.kill().unwrap();
    provider.wait().unwrap();
    fixture.finish().unwrap();

    let mut daemon = TestDaemon::start(&directory).unwrap();
    let create = daemon
        .provider("CreateInstance", None, Some(&bootstrap_json().unwrap()))
        .unwrap();
    assert_ne!(create.status.code(), Some(0));
    let connection = Connection::open(&daemon.database).unwrap();
    let allocations: i64 = connection
        .query_row("SELECT COUNT(*) FROM allocations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        allocations, 0,
        "missing snapshot must fail before reservation"
    );
    let delete = daemon
        .provider("DeleteInstance", Some("runner-test"), None)
        .unwrap();
    assert_eq!(delete.status.code(), Some(30));
    assert!(delete.stdout.is_empty());
    let connection = Connection::open(&daemon.database).unwrap();
    let fences: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM allocation_delete_fences WHERE controller_id=?1 AND identity_key=?2",
            rusqlite::params!["controller-test", "runner-test"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fences, 1, "absent delete must persist its identity fence");
    daemon.stop().unwrap();

    let mut restarted = TestDaemon::start(&directory).unwrap();
    let connection = Connection::open(&restarted.database).unwrap();
    let allocations: i64 = connection
        .query_row("SELECT COUNT(*) FROM allocations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        allocations, 0,
        "unowned delete must not create an allocation row"
    );
    let connection = Connection::open(&restarted.database).unwrap();
    let fences: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM allocation_delete_fences WHERE controller_id=?1 AND identity_key=?2",
            rusqlite::params!["controller-test", "runner-test"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fences, 1, "absent delete fence must survive restart");
    restarted.stop().unwrap();
}

#[test]
fn not_found_and_http_errors() {
    let directory = TestDirectory::new().unwrap();
    let not_found = HttpFixture::start(
        404,
        provider_response(None, Some("absent"), true).unwrap(),
        false,
    )
    .unwrap();
    let config = write_provider_config(&directory, &not_found.address, 5).unwrap();
    let delete = provider_process(
        &config,
        "DeleteInstance",
        None,
        Some("missing"),
        Some("pool-test"),
        None,
        None,
    )
    .unwrap();
    assert_eq!(delete.status.code(), Some(30));
    assert!(delete.stdout.is_empty());
    not_found.request().unwrap();
    not_found.finish().unwrap();

    let bad_json = HttpFixture::start(200, "{malformed".into(), false).unwrap();
    let config = write_provider_config(&directory, &bad_json.address, 5).unwrap();
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
    bad_json.request().unwrap();
    bad_json.finish().unwrap();

    let failed =
        HttpFixture::start(404, provider_response(None, None, false).unwrap(), false).unwrap();
    let config = write_provider_config(&directory, &failed.address, 5).unwrap();
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
    failed.request().unwrap();
    failed.finish().unwrap();
}

#[test]
fn provider_rejects_malformed_or_conflicting_snapshot_sources() {
    let directory = TestDirectory::new().unwrap();
    let config = write_provider_config(&directory, "127.0.0.1:1", 1).unwrap();
    for invalid in [
        json!(null),
        json!(12),
        json!(""),
        json!([]),
        json!({}),
        json!("other-id"),
    ] {
        for bootstrap_invalid in [false, true] {
            let mut bootstrap: Value = serde_json::from_slice(&bootstrap_json().unwrap()).unwrap();
            let mut environment = json!({"snapshot_id":"snapshot-id"});
            bootstrap["extra_specs"] = json!({"snapshot_id":"snapshot-id"});
            if bootstrap_invalid {
                bootstrap["extra_specs"] = json!({"snapshot_id":invalid});
            } else {
                environment = json!({"snapshot_id":invalid});
            }

            let extras = STANDARD.encode(serde_json::to_vec(&environment).unwrap());
            let output = provider_process(
                &config,
                "CreateInstance",
                Some("v0.1.1"),
                None,
                Some("pool-1"),
                Some(&extras),
                Some(&serde_json::to_vec(&bootstrap).unwrap()),
            )
            .unwrap();
            assert_ne!(output.status.code(), Some(0));
            assert!(output.stdout.is_empty());
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("invalid or conflicting pool snapshot IDs")
            );
        }
    }
}

#[test]
fn snapshot_resolution_ignores_unrelated_template_field_differences() {
    let directory = TestDirectory::new().unwrap();
    let fixture = HttpFixture::start(
        503,
        provider_response(None, Some("snapshot not registered"), false).unwrap(),
        false,
    )
    .unwrap();
    let config = write_provider_config(&directory, &fixture.address, 5).unwrap();
    let mut bootstrap: Value = serde_json::from_slice(&bootstrap_json().unwrap()).unwrap();
    bootstrap["extra_specs"] = json!({"snapshot_id":"snapshot-id", "template_field":"bootstrap"});
    let extras =
        STANDARD.encode(br#"{"snapshot_id":"snapshot-id","template_field":"environment"}"#);
    let output = provider_process(
        &config,
        "CreateInstance",
        Some("v0.1.1"),
        None,
        Some("pool-1"),
        Some(&extras),
        Some(&serde_json::to_vec(&bootstrap).unwrap()),
    )
    .unwrap();
    assert_ne!(output.status.code(), Some(0));
    let request = String::from_utf8(fixture.request().unwrap()).unwrap();
    let request_json: Value =
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(
        request_json["pool_extra_specs"]["snapshot_id"],
        "snapshot-id"
    );
    assert_eq!(
        request_json["bootstrap"]["extra_specs"]["template_field"],
        "bootstrap"
    );
    fixture.finish().unwrap();
}
