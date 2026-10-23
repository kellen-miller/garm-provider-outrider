use super::cleanup_scenario::TempJournal;
use super::native_scope::{NativeCloudScope, native_cloud_fixture};

use std::{thread, time::Duration};

use outrider::{cleanup::CleanupInspection, journal::Journal};
use serde_json::json;

use super::cleanup_scenario::{attestation, cli, fixture, invoke_inspect, invoke_resolve, now};
#[test]
fn inconsistent_native_scope_is_rejected_without_releasing_cap_or_audit() {
    let fixture = native_cloud_fixture("inconsistent-native", NativeCloudScope::Azure);
    let connection = rusqlite::Connection::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("open inconsistent native fixture: {error}"));
    let mut identity: serde_json::Value = connection
        .query_row(
            "SELECT native_identity_json FROM allocations WHERE allocation_id='allocation-1'",
            [],
            |row| row.get::<_, String>(0),
        )
        .and_then(|value| serde_json::from_str(&value).map_err(|_| rusqlite::Error::InvalidQuery))
        .unwrap_or_else(|error| panic!("read inconsistent native identity: {error}"));
    identity["location"] = json!("westus3");
    connection
        .execute(
            "UPDATE allocations SET native_identity_json=?2 WHERE allocation_id=?1",
            rusqlite::params![
                "allocation-1",
                serde_json::to_string(&identity).unwrap_or_else(|error| panic!(
                    "serialize inconsistent native identity: {error}"
                ))
            ],
        )
        .unwrap_or_else(|error| panic!("persist inconsistent native identity: {error}"));
    drop(connection);

    let rejected = invoke_inspect(&fixture);
    assert!(!rejected.status.success());
    let journal = Journal::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("reopen inconsistent native fixture: {error}"));
    assert_eq!(
        journal
            .active_allocation_counts("controller-1", "pool-1")
            .unwrap_or_else(|error| panic!("read inconsistent native caps: {error}")),
        (1, 1)
    );
    drop(journal);
    let connection = rusqlite::Connection::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("read inconsistent native audit: {error}"));
    let audit_count: u32 = connection
        .query_row("SELECT COUNT(*) FROM cleanup_resolution_audit", [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|error| panic!("count inconsistent native audit: {error}"));
    assert_eq!(audit_count, 0);
}

#[test]
fn proof_before_latest_child_completion_is_rejected_while_fresh() {
    let mut fixture = fixture("completion-time");
    let checked_at = now().saturating_sub(30);
    let connection = rusqlite::Connection::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("open completion-time fixture: {error}"));
    connection
        .execute(
            "UPDATE allocations SET dispatched_at=?2 WHERE allocation_id=?1",
            rusqlite::params![
                "allocation-1",
                i64::try_from(checked_at - 30)
                    .unwrap_or_else(|error| panic!("convert allocation dispatch time: {error}"))
            ],
        )
        .unwrap_or_else(|error| panic!("set allocation dispatch fixture time: {error}"));
    connection
        .execute(
            "UPDATE child_attempts SET dispatched_at=?2, completed_at=?3 WHERE allocation_id=?1",
            rusqlite::params![
                "allocation-1",
                i64::try_from(checked_at - 10)
                    .unwrap_or_else(|error| panic!("convert child dispatch time: {error}")),
                i64::try_from(checked_at + 10)
                    .unwrap_or_else(|error| panic!("convert child completion time: {error}"))
            ],
        )
        .unwrap_or_else(|error| panic!("set child attempt fixture times: {error}"));
    connection
        .execute(
            "UPDATE allocation_actions SET started_at=?2, completed_at=?3, last_attempt_at=?3 WHERE allocation_id=?1",
            rusqlite::params![
                "allocation-1",
                i64::try_from(checked_at - 20)
                    .unwrap_or_else(|error| panic!("convert lifecycle start time: {error}")),
                i64::try_from(checked_at - 5)
                    .unwrap_or_else(|error| panic!("convert lifecycle completion time: {error}"))
            ],
        )
        .unwrap_or_else(|error| panic!("set lifecycle fixture times: {error}"));
    drop(connection);
    let journal = Journal::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("inspect completion-time fixture: {error}"));
    fixture.inspection = outrider::cleanup::inspect(&journal, "controller-1", "allocation-1")
        .unwrap_or_else(|error| panic!("read completion-time inspection: {error}"));
    drop(journal);
    let mut proof = attestation(&fixture.inspection);
    proof.verified_at_unix_seconds = checked_at;

    assert!(!invoke_resolve(&fixture, &proof).status.success());
    let journal = Journal::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("reopen completion-time fixture: {error}"));
    assert_eq!(
        journal
            .active_allocation_counts("controller-1", "pool-1")
            .unwrap_or_else(|error| panic!("read completion-time caps: {error}")),
        (1, 1)
    );
}

#[test]
fn invalid_ack_scope_time_and_evidence_leave_cleanup_owned() {
    let fixture = fixture("invalid-proof");
    let mut invalid_proofs = Vec::new();
    let mut wrong_pool = attestation(&fixture.inspection);
    wrong_pool.pool_id = "different-pool".into();
    invalid_proofs.push(wrong_pool);
    let mut missing_acknowledgement = attestation(&fixture.inspection);
    missing_acknowledgement
        .acknowledgements
        .all_cloud_operations_terminal = false;
    invalid_proofs.push(missing_acknowledgement);
    let mut old_time = attestation(&fixture.inspection);
    old_time.verified_at_unix_seconds = now().saturating_sub(3601);
    invalid_proofs.push(old_time);
    let mut future_time = attestation(&fixture.inspection);
    future_time.verified_at_unix_seconds = now().saturating_add(60);
    invalid_proofs.push(future_time);
    let mut invalid_reference = attestation(&fixture.inspection);
    invalid_reference.evidence_reference = "/private/evidence-file".into();
    invalid_proofs.push(invalid_reference);

    for (index, proof) in invalid_proofs.into_iter().enumerate() {
        let rejected = invoke_resolve(&fixture, &proof);
        assert!(
            !rejected.status.success(),
            "invalid proof {index} accepted: {}",
            String::from_utf8_lossy(&rejected.stdout)
        );
        let journal = Journal::open(&fixture.temp.path)
            .unwrap_or_else(|error| panic!("reopen invalid-proof journal: {error}"));
        assert_eq!(
            journal
                .allocation_by_id("allocation-1")
                .unwrap_or_else(|error| panic!("read invalid-proof allocation: {error}"))
                .map(|allocation| allocation.state),
            Some("deleting".into())
        );
        assert_eq!(
            journal
                .active_allocation_counts("controller-1", "pool-1")
                .unwrap(),
            (1, 1)
        );
        drop(journal);
    }
    let connection = rusqlite::Connection::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("read invalid-proof audit: {error}"));
    let audit_count: u32 = connection
        .query_row("SELECT COUNT(*) FROM cleanup_resolution_audit", [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|error| panic!("count invalid-proof audit: {error}"));
    assert_eq!(audit_count, 0);
}

#[test]
fn changed_positive_return_invalidates_inspection_proof() {
    let fixture = fixture("stale");
    let output = invoke_inspect(&fixture);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let inspection: CleanupInspection = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("parse stale inspection: {error}"));
    let proof = attestation(&inspection);

    let journal = Journal::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("open stale fixture journal: {error}"));
    journal
        .preserve_ambiguous_child_result("allocation-1", "{}", "late positive return")
        .unwrap_or_else(|error| panic!("retain late result: {error}"));
    drop(journal);

    let rejected = invoke_resolve(&fixture, &proof);
    assert!(!rejected.status.success());
    let journal = Journal::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("reopen stale fixture journal: {error}"));
    assert_eq!(
        journal
            .allocation_by_id("allocation-1")
            .unwrap_or_else(|error| panic!("read stale allocation: {error}"))
            .map(|allocation| allocation.state),
        Some("deleting".into())
    );
    let connection = rusqlite::Connection::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("read stale audit: {error}"));
    let audit_count: u32 = connection
        .query_row("SELECT COUNT(*) FROM cleanup_resolution_audit", [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|error| panic!("count stale audit: {error}"));
    assert_eq!(audit_count, 0);
}

#[test]
fn cleanup_cli_rejects_missing_database_without_creating_it() {
    let temp = TempJournal::new("missing");
    let output = cli(&[
        "cleanup",
        "inspect",
        "--database",
        temp.path.to_str().unwrap_or_default(),
        "--controller",
        "controller-1",
        "--allocation",
        "allocation-1",
    ]);
    assert!(!output.status.success());
    assert!(!temp.path.exists());
}

#[test]
fn scope_digest_changes_when_action_attempt_generation_changes() {
    let fixture = fixture("generation");
    let first = fixture.inspection.scope_digest;
    thread::sleep(Duration::from_millis(1_100));
    let journal = Journal::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("open generation fixture: {error}"));
    let action = journal
        .begin_allocation_action("controller-1", "runner-1", "DeleteInstance")
        .unwrap_or_else(|error| panic!("retry generation action: {error}"))
        .unwrap_or_else(|| panic!("missing retry action"));
    if action.acquired {
        let action_id = action
            .action_id
            .unwrap_or_else(|| panic!("missing retry action ID"));
        journal
            .finish_allocation_action(action_id, "DeleteInstance", "ambiguous", Some("retry"))
            .unwrap_or_else(|error| panic!("finish retry generation: {error}"));
    }
    let second = outrider::cleanup::inspect(&journal, "controller-1", "allocation-1")
        .unwrap_or_else(|error| panic!("inspect new generation: {error}"))
        .scope_digest;
    assert_ne!(first, second);
}
