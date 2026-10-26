#![cfg(unix)]

use outrider::journal::Journal;

use super::journal_process::journal_path;
#[test]
fn schema_v11_migrates_without_losing_allocations_and_unknown_versions_fail() {
    let database = journal_path();
    let journal = Journal::open(&database).expect("open current journal");
    journal
        .reserve_allocation(
            "opaque-preserved",
            "controller",
            "runner",
            "snapshot",
            "pool",
        )
        .expect("insert allocation");
    drop(journal);

    let connection = rusqlite::Connection::open(&database).expect("open SQLite directly");
    connection
        .execute_batch(
            "DROP TRIGGER cleanup_resolution_audit_no_update;
             DROP TRIGGER cleanup_resolution_audit_no_delete;
             DROP TABLE cleanup_resolution_audit;
             DROP TABLE allocation_delete_fences;
             DROP INDEX allocation_actions_reusable_delete;
             ALTER TABLE allocation_actions DROP COLUMN next_attempt_at;
             ALTER TABLE allocation_actions DROP COLUMN last_attempt_at;
             ALTER TABLE allocation_actions DROP COLUMN attempt_count;
             ALTER TABLE allocation_actions DROP COLUMN reusable;
             UPDATE schema_version SET version=11;",
        )
        .expect("set legacy schema version");
    drop(connection);
    let journal = Journal::open(&database).expect("migrate v11 journal");
    assert_eq!(
        journal
            .allocation("controller", "runner")
            .expect("read migrated row")
            .expect("preserved allocation")
            .allocation_id,
        "opaque-preserved"
    );
    drop(journal);

    let connection = rusqlite::Connection::open(&database).expect("open migrated SQLite");
    let version: u32 = connection
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .expect("read schema version");
    assert_eq!(version, 16);
    connection
        .execute("UPDATE schema_version SET version=99", [])
        .expect("set unsupported schema version");
    drop(connection);
    assert!(
        Journal::open(&database).is_err(),
        "unsupported version must fail"
    );
    std::fs::remove_file(&database).expect("remove test database");
    std::fs::remove_file(database.with_extension("lock")).expect("remove test lock");
}

#[test]
fn cleanup_retry_is_one_durable_due_gated_action() {
    let database = journal_path();
    let journal = Journal::open(&database).expect("open journal");
    journal
        .reserve_allocation("cleanup", "controller", "runner", "snapshot", "pool")
        .expect("reserve allocation");
    let connection = rusqlite::Connection::open(&database).expect("open fixture database");
    connection
        .execute(
            "UPDATE allocations SET state='dispatched', dispatched_at=100 WHERE allocation_id='cleanup'",
            [],
        )
        .expect("model dispatched create");
    drop(connection);
    journal
        .schedule_allocation_cleanup("cleanup", "first delete intent")
        .expect("schedule cleanup");
    let first = journal
        .first_allocation_delete_intent("cleanup")
        .expect("read first delete intent")
        .expect("retain first delete intent");
    let admission = journal
        .begin_allocation_action("controller", "cleanup", "DeleteInstance")
        .expect("admit first cleanup attempt")
        .expect("allocation exists");
    assert!(admission.acquired);
    assert!(
        journal
            .finish_allocation_action(
                admission.action_id.expect("reusable action"),
                "DeleteInstance",
                "ambiguous",
                Some("cleanup remains unresolved"),
            )
            .expect("persist unresolved result")
    );
    journal
        .schedule_allocation_cleanup("cleanup", "repeated schedule")
        .expect("repeat cleanup schedule");
    journal
        .reserve_allocation(
            "independent",
            "controller",
            "runner-independent",
            "snapshot",
            "pool",
        )
        .expect("reserve independent allocation");
    let connection = rusqlite::Connection::open(&database).expect("open due-filter fixture");
    connection
        .execute(
            "UPDATE allocations SET state='dispatched', dispatched_at=101 WHERE allocation_id='independent'",
            [],
        )
        .expect("model independent dispatched create");
    drop(connection);
    journal
        .schedule_allocation_cleanup("independent", "independent due cleanup")
        .expect("schedule independent cleanup");
    let connection = rusqlite::Connection::open(&database).expect("set poison backoff");
    connection
        .execute(
            "UPDATE allocation_actions SET next_attempt_at=CAST(strftime('%s','now') AS INTEGER)+300 WHERE allocation_id='cleanup' AND reusable=1",
            [],
        )
        .expect("keep first cleanup not due");
    drop(connection);
    let due = journal
        .recoverable_allocations()
        .expect("filter non-due cleanup before recovery limit");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].allocation_id, "independent");
    let delayed = journal
        .begin_allocation_action("controller", "cleanup", "DeleteInstance")
        .expect("check backoff")
        .expect("allocation exists");
    assert!(
        !delayed.acquired,
        "direct admission must honor retry due time"
    );
    assert_eq!(
        journal
            .first_allocation_delete_intent("cleanup")
            .expect("read immutable first intent"),
        Some(first)
    );
    drop(journal);

    let connection = rusqlite::Connection::open(&database).expect("inspect durable retry");
    let durable: (i64, i64, i64, String) = connection
        .query_row(
            "SELECT attempt_count, last_attempt_at, next_attempt_at, state FROM allocation_actions WHERE allocation_id='cleanup' AND reusable=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read durable attempt fields");
    assert_eq!(durable.0, 1);
    assert!(durable.2 > durable.1);
    assert_eq!(durable.3, "pending");
    connection
        .execute(
            "UPDATE allocation_actions SET next_attempt_at=0 WHERE allocation_id='cleanup' AND reusable=1",
            [],
        )
        .expect("make retry due through fixture SQL");
    drop(connection);

    let reopened = Journal::open(&database).expect("reopen durable retry");
    let retry = reopened
        .begin_allocation_action("controller", "cleanup", "DeleteInstance")
        .expect("admit second cleanup attempt")
        .expect("allocation exists");
    assert!(retry.acquired);
    assert_eq!(retry.action_id, admission.action_id);
    assert_eq!(
        reopened
            .first_allocation_delete_intent("cleanup")
            .expect("first intent survives reopen"),
        Some(first)
    );
    assert_eq!(
        reopened
            .active_allocation_counts("controller", "pool")
            .expect("unknown cleanup still counts against caps"),
        (2, 2)
    );
    drop(reopened);
    std::fs::remove_file(&database).expect("remove test database");
    std::fs::remove_file(database.with_extension("lock")).expect("remove test lock");
}

#[test]
fn schema_v12_migration_preserves_cleanup_tombstones_and_native_data() {
    let database = journal_path();
    let journal = Journal::open(&database).expect("open current journal");
    journal
        .reserve_allocation("cleanup-v12", "controller", "runner", "snapshot", "pool")
        .expect("reserve allocation");
    let connection = rusqlite::Connection::open(&database).expect("open legacy allocation row");
    connection
        .execute(
            "UPDATE allocations SET state='dispatched', dispatched_at=100 WHERE allocation_id='cleanup-v12'",
            [],
        )
        .expect("model dispatched create");
    drop(connection);
    journal
        .schedule_allocation_cleanup("cleanup-v12", "legacy cleanup intent")
        .expect("persist legacy cleanup");
    let first = journal
        .first_allocation_delete_intent("cleanup-v12")
        .expect("read legacy first intent")
        .expect("legacy delete intent exists");
    drop(journal);

    let connection = rusqlite::Connection::open(&database).expect("open SQLite directly");
    connection
        .execute_batch(
            "DROP TRIGGER cleanup_resolution_audit_no_update;
             DROP TRIGGER cleanup_resolution_audit_no_delete;
             DROP TABLE cleanup_resolution_audit;
             UPDATE allocations SET native_identity_json='legacy-native-identity' WHERE allocation_id='cleanup-v12';
             DROP INDEX allocation_actions_reusable_delete;
             ALTER TABLE allocation_actions DROP COLUMN next_attempt_at;
             ALTER TABLE allocation_actions DROP COLUMN last_attempt_at;
             ALTER TABLE allocation_actions DROP COLUMN attempt_count;
             ALTER TABLE allocation_actions DROP COLUMN reusable;
             UPDATE schema_version SET version=12;",
        )
        .expect("restore exact v12 table shape");
    drop(connection);

    let migrated = Journal::open(&database).expect("migrate v12 cleanup journal");
    let allocation = migrated
        .allocation("controller", "runner")
        .expect("read migrated allocation")
        .expect("allocation preserved");
    assert!(allocation.tombstone);
    assert_eq!(allocation.state, "deleting");
    assert_eq!(
        allocation.native_identity_json.as_deref(),
        Some("legacy-native-identity")
    );
    assert_eq!(
        migrated
            .first_allocation_delete_intent("cleanup-v12")
            .expect("legacy first intent survives migration"),
        Some(first)
    );
    drop(migrated);

    let connection = rusqlite::Connection::open(&database).expect("read migrated version");
    let version: u32 = connection
        .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
        .expect("read schema version");
    assert_eq!(version, 16);
    drop(connection);
    std::fs::remove_file(&database).expect("remove test database");
    std::fs::remove_file(database.with_extension("lock")).expect("remove test lock");
}

#[test]
fn schema_v13_migration_adds_append_only_cleanup_audit() {
    let database = journal_path();
    let journal = Journal::open(&database).expect("open current journal");
    journal
        .reserve_allocation("audit-row", "controller", "runner", "snapshot", "pool")
        .expect("insert audit allocation");
    drop(journal);

    let connection = rusqlite::Connection::open(&database).expect("open SQLite directly");
    connection
        .execute_batch(
            "DROP TRIGGER cleanup_resolution_audit_no_update;
             DROP TRIGGER cleanup_resolution_audit_no_delete;
             DROP TABLE cleanup_resolution_audit;
             UPDATE schema_version SET version=13;",
        )
        .expect("restore exact v13 schema shape");
    drop(connection);

    let journal = Journal::open(&database).expect("migrate v13 journal");
    drop(journal);
    let connection = rusqlite::Connection::open(&database).expect("read migrated audit schema");
    connection
        .execute(
            "INSERT INTO cleanup_resolution_audit(allocation_id, attestation_digest, evidence_reference, verified_at, committed_at, uid, gid, summary_json) VALUES ('audit-row', 'digest', 'ticket:INC-1', 1, 2, 3, 4, '{}')",
            [],
        )
        .expect("insert audit row");
    assert!(
        connection
            .execute(
                "UPDATE cleanup_resolution_audit SET evidence_reference='changed'",
                []
            )
            .is_err()
    );
    assert!(
        connection
            .execute("DELETE FROM cleanup_resolution_audit", [])
            .is_err()
    );
    drop(connection);
    std::fs::remove_file(&database).expect("remove test database");
    std::fs::remove_file(database.with_extension("lock")).expect("remove test lock");
}
