use std::{
    fs,
    process::{Command, Stdio},
    time::Duration,
};

use rusqlite::Connection;
use serde_json::{Value, json};

use super::daemon_scenario::{BOOTSTRAP_TOKEN, INNER_TOKEN, TestDaemon, TestDirectory};
use super::provider_wire::{
    HttpFixture, bootstrap_json, provider_process, provider_response, write_provider_config,
};

#[test]
fn v010_create_uses_snapshot_id_from_bootstrap_extra_specs() {
    let directory = TestDirectory::new().unwrap();
    let fixture = HttpFixture::start(
        503,
        provider_response(None, Some("durable pool snapshot is not registered"), false).unwrap(),
        false,
    )
    .unwrap();
    let config = write_provider_config(&directory, &fixture.address, 5).unwrap();
    let mut bootstrap: Value = serde_json::from_slice(&bootstrap_json().unwrap()).unwrap();
    bootstrap["extra_specs"] = json!({
        "snapshot_id":"opaque-snapshot-id",
        "template_field":"allowed-by-garm"
    });
    let output = provider_process(
        &config,
        "CreateInstance",
        Some("v0.1.0"),
        None,
        None,
        None,
        Some(&serde_json::to_vec(&bootstrap).unwrap()),
    )
    .unwrap();
    assert_ne!(output.status.code(), Some(0));
    let request = String::from_utf8(fixture.request().unwrap()).unwrap();
    let request_body = request.split_once("\r\n\r\n").unwrap().1;
    let request_json: Value = serde_json::from_str(request_body).unwrap();
    assert_eq!(
        request_json["pool_extra_specs"]["snapshot_id"],
        "opaque-snapshot-id"
    );
    assert_eq!(
        request_json["pool_extra_specs"]["template_field"],
        "allowed-by-garm"
    );
    fixture.finish().unwrap();
}

#[test]
fn duplicate_restart() {
    let directory = TestDirectory::new().unwrap();
    let mut daemon = TestDaemon::start(&directory).unwrap();
    let output = daemon
        .provider("CreateInstance", None, Some(&bootstrap_json().unwrap()))
        .unwrap();
    assert_ne!(output.status.code(), Some(0));
    daemon.stop().unwrap();
    let mut restarted = TestDaemon::start(&directory).unwrap();
    let connection = Connection::open(&restarted.database).unwrap();
    let allocations: i64 = connection
        .query_row("SELECT COUNT(*) FROM allocations", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        allocations, 0,
        "missing snapshot must fail before reservation"
    );
    let duplicate = restarted
        .provider("CreateInstance", None, Some(&bootstrap_json().unwrap()))
        .unwrap();
    assert_ne!(duplicate.status.code(), Some(0));
    restarted.stop().unwrap();
}

#[test]
fn exclusive_daemon() {
    let directory = TestDirectory::new().unwrap();
    let mut daemon = TestDaemon::start(&directory).unwrap();
    let duplicate = Command::new(env!("CARGO_BIN_EXE_outrider"))
        .args(["serve", "--config"])
        .arg(&daemon.config)
        .output()
        .unwrap();
    assert_ne!(duplicate.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&duplicate.stderr);
    assert!(
        stderr.contains("already running"),
        "unexpected duplicate daemon error: {stderr}"
    );
    daemon.stop().unwrap();
}

#[test]
fn provider_deadline() {
    let directory = TestDirectory::new().unwrap();
    let fixture =
        HttpFixture::start(200, provider_response(None, None, false).unwrap(), true).unwrap();
    let config = write_provider_config(&directory, &fixture.address, 1).unwrap();
    let started = std::time::Instant::now();
    let child = Command::new(env!("CARGO_BIN_EXE_outrider"))
        .env("GARM_COMMAND", "GetInstance")
        .env("GARM_CONTROLLER_ID", "controller-test")
        .env("GARM_INSTANCE_ID", "runner-test")
        .env("GARM_PROVIDER_CONFIG_FILE", config)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let request = fixture.request().unwrap();
    assert!(String::from_utf8_lossy(&request).contains("POST /v1/provider"));
    let output = child.wait_with_output().unwrap();
    assert_ne!(output.status.code(), Some(0));
    assert!(started.elapsed() < Duration::from_secs(3));
    fixture.finish().unwrap();
}

#[test]
fn malformed_and_redaction() {
    let directory = TestDirectory::new().unwrap();
    let mut daemon = TestDaemon::start(&directory).unwrap();
    let malformed = daemon
        .provider(
            "CreateInstance",
            None,
            Some(b"{\"instance-token\":\"malformed-secret\""),
        )
        .unwrap();
    assert_ne!(malformed.status.code(), Some(0));
    assert!(malformed.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&malformed.stderr);
    assert!(!stderr.contains("malformed-secret"));
    let accepted = daemon
        .provider("CreateInstance", None, Some(&bootstrap_json().unwrap()))
        .unwrap();
    assert_ne!(accepted.status.code(), Some(0));
    let daemon_stderr = daemon.stop().unwrap();
    assert!(!daemon_stderr.contains(BOOTSTRAP_TOKEN));
    assert!(!daemon_stderr.contains(INNER_TOKEN));
    assert!(!daemon_stderr.contains("ca-secret-canary"));
    for path in [
        directory.path().join("journal.sqlite3"),
        directory.path().join("journal.sqlite3-wal"),
    ] {
        if let Ok(bytes) = fs::read(path) {
            let content = String::from_utf8_lossy(&bytes);
            assert!(!content.contains(BOOTSTRAP_TOKEN));
            assert!(!content.contains("ca-secret-canary"));
            assert!(!content.contains(INNER_TOKEN));
        }
    }
}
