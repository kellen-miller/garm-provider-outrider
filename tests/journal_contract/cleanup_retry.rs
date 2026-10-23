#![cfg(unix)]

use outrider::journal::Journal;

use super::journal_process::{journal_path, unix_seconds};
#[test]
fn only_new_positive_create_evidence_promotes_retry_due_time() {
    let database = journal_path();
    let journal = Journal::open(&database).expect("open journal");
    journal
        .reserve_allocation("late-positive", "controller", "runner", "snapshot", "pool")
        .expect("reserve allocation");
    let connection = rusqlite::Connection::open(&database).expect("open allocation fixture");
    connection
        .execute(
            "UPDATE allocations SET state='dispatched', dispatched_at=100 WHERE allocation_id='late-positive'",
            [],
        )
        .expect("model dispatched create");
    drop(connection);
    journal
        .schedule_allocation_cleanup("late-positive", "cleanup intent")
        .expect("schedule cleanup");
    let admission = journal
        .begin_allocation_action("controller", "late-positive", "DeleteInstance")
        .expect("admit delete")
        .expect("allocation exists");
    journal
        .preserve_ambiguous_child_result(
            "late-positive",
            r#"{"provider_id":"i-first"}"#,
            "late positive child return",
        )
        .expect("persist positive child evidence during cleanup");
    journal
        .finish_allocation_action(
            admission.action_id.expect("reusable action"),
            "DeleteInstance",
            "ambiguous",
            Some("cleanup remains unresolved"),
        )
        .expect("finish guarded attempt");
    let connection = rusqlite::Connection::open(&database).expect("inspect promoted retry");
    let due: i64 = connection
        .query_row(
            "SELECT next_attempt_at FROM allocation_actions WHERE allocation_id='late-positive' AND reusable=1",
            [],
            |row| row.get(0),
        )
        .expect("read retry due time");
    assert!(due <= unix_seconds());
    connection
        .execute(
            "UPDATE allocation_actions SET next_attempt_at=CAST(strftime('%s','now') AS INTEGER)+300 WHERE allocation_id='late-positive' AND reusable=1",
            [],
        )
        .expect("simulate renewed backoff");
    drop(connection);
    journal
        .preserve_ambiguous_child_result(
            "late-positive",
            r#"{"provider_id":"i-new"}"#,
            "different response after first positive return",
        )
        .expect("retain the first positive child return");
    let connection = rusqlite::Connection::open(&database).expect("inspect unchanged retry");
    let unchanged: i64 = connection
        .query_row(
            "SELECT next_attempt_at FROM allocation_actions WHERE allocation_id='late-positive' AND reusable=1",
            [],
            |row| row.get(0),
        )
        .expect("read unchanged due time");
    assert!(unchanged > unix_seconds());
    drop(connection);
    let allocation = journal
        .allocation_by_id("late-positive")
        .expect("read retained child result")
        .expect("allocation exists");
    assert_eq!(
        allocation.provider_instance_json.as_deref(),
        Some(r#"{"provider_id":"i-first"}"#)
    );
    let identity = outrider::cloud::PlacementIdentity {
        controller_id: "controller".to_owned(),
        cloud: "aws".to_owned(),
        account_id: "account".to_owned(),
        location: "region".to_owned(),
        resource_id: "i-first".to_owned(),
        host_id: None,
        dependency_ids: Default::default(),
    };
    journal
        .claim_allocation_native_identity("late-positive", &identity)
        .expect("claim first authoritative native identity");
    let connection = rusqlite::Connection::open(&database).expect("inspect new retry");
    let promoted: i64 = connection
        .query_row(
            "SELECT next_attempt_at FROM allocation_actions WHERE allocation_id='late-positive' AND reusable=1",
            [],
            |row| row.get(0),
        )
        .expect("read promoted due time");
    assert!(promoted <= unix_seconds());
    drop(connection);
    let connection = rusqlite::Connection::open(&database).expect("restore test backoff");
    connection
        .execute(
            "UPDATE allocation_actions SET next_attempt_at=CAST(strftime('%s','now') AS INTEGER)+300 WHERE allocation_id='late-positive' AND reusable=1",
            [],
        )
        .expect("set future due time");
    drop(connection);
    journal
        .claim_allocation_native_identity("late-positive", &identity)
        .expect("repeat identical native claim");
    let connection = rusqlite::Connection::open(&database).expect("inspect duplicate native claim");
    let unchanged: i64 = connection
        .query_row(
            "SELECT next_attempt_at FROM allocation_actions WHERE allocation_id='late-positive' AND reusable=1",
            [],
            |row| row.get(0),
        )
        .expect("read retry due time");
    assert!(unchanged > unix_seconds());
    drop(connection);
    drop(journal);
    std::fs::remove_file(&database).expect("remove test database");
    std::fs::remove_file(database.with_extension("lock")).expect("remove test lock");
}

#[test]
fn cleanup_backoff_is_exponential_capped_and_keeps_one_action() {
    let database = journal_path();
    let journal = Journal::open(&database).expect("open journal");
    journal
        .reserve_allocation("backoff", "controller", "runner", "snapshot", "pool")
        .expect("reserve allocation");
    let connection = rusqlite::Connection::open(&database).expect("open allocation fixture");
    connection
        .execute(
            "UPDATE allocations SET state='dispatched', dispatched_at=100 WHERE allocation_id='backoff'",
            [],
        )
        .expect("model dispatched create");
    drop(connection);
    journal
        .schedule_allocation_cleanup("backoff", "first delete intent")
        .expect("schedule cleanup");
    let first_intent = journal
        .first_allocation_delete_intent("backoff")
        .expect("read first delete intent")
        .expect("first intent exists");
    let initial = journal
        .begin_allocation_action("controller", "backoff", "DeleteInstance")
        .expect("admit first attempt")
        .expect("allocation exists");
    let action_id = initial.action_id.expect("one reusable action");
    let backoffs = [1_i64, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300];
    for (index, seconds) in backoffs.into_iter().enumerate() {
        if index > 0 {
            let connection = rusqlite::Connection::open(&database).expect("make retry due");
            connection
                .execute(
                    "UPDATE allocation_actions SET next_attempt_at=0 WHERE allocation_id='backoff' AND reusable=1",
                    [],
                )
                .expect("force due time in fixture");
            drop(connection);
        }
        let admission = if index == 0 {
            initial.clone()
        } else {
            journal
                .begin_allocation_action("controller", "backoff", "DeleteInstance")
                .expect("admit retry")
                .expect("allocation exists")
        };
        assert!(admission.acquired);
        assert_eq!(admission.action_id, Some(action_id));
        journal
            .finish_allocation_action(
                action_id,
                "DeleteInstance",
                "ambiguous",
                Some("cleanup remains unresolved"),
            )
            .expect("persist retry outcome");
        let connection = rusqlite::Connection::open(&database).expect("inspect retry timing");
        let attempt: (i64, i64, i64, i64, i64) = connection
            .query_row(
                "SELECT attempt_count, last_attempt_at, completed_at, next_attempt_at, (SELECT COUNT(*) FROM allocation_actions WHERE allocation_id='backoff') FROM allocation_actions WHERE allocation_id='backoff' AND reusable=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .expect("read attempt count and timestamps");
        assert_eq!(attempt.0, i64::try_from(index + 1).unwrap_or(i64::MAX));
        assert!(attempt.2 >= attempt.1);
        assert_eq!(attempt.3 - attempt.2, seconds);
        assert_eq!(attempt.4, 1, "retry appends no action row");
        drop(connection);
        assert_eq!(
            journal
                .first_allocation_delete_intent("backoff")
                .expect("first intent remains immutable"),
            Some(first_intent)
        );
        assert_eq!(
            journal
                .active_allocation_counts("controller", "pool")
                .expect("uncertain cleanup remains capped"),
            (1, 1)
        );
    }
    drop(journal);
    std::fs::remove_file(&database).expect("remove test database");
    std::fs::remove_file(database.with_extension("lock")).expect("remove test lock");
}

#[test]
fn startup_preserves_interrupted_retry_and_evidence_wake() {
    let database = journal_path();
    let journal = Journal::open(&database).expect("open journal");
    for allocation_id in ["startup-normal", "startup-woken"] {
        journal
            .reserve_allocation(
                allocation_id,
                "controller",
                allocation_id,
                "snapshot",
                "pool",
            )
            .expect("reserve allocation");
    }
    let connection = rusqlite::Connection::open(&database).expect("open allocation fixtures");
    connection
        .execute(
            "UPDATE allocations SET state='dispatched', dispatched_at=100 WHERE allocation_id IN ('startup-normal','startup-woken')",
            [],
        )
        .expect("model dispatched creates");
    drop(connection);
    let mut action_ids = Vec::new();
    let mut first_intents = Vec::new();
    for allocation_id in ["startup-normal", "startup-woken"] {
        journal
            .schedule_allocation_cleanup(allocation_id, "startup cleanup")
            .expect("schedule cleanup");
        first_intents.push(
            journal
                .first_allocation_delete_intent(allocation_id)
                .expect("read first delete intent")
                .expect("first delete intent exists"),
        );
        let admission = journal
            .begin_allocation_action("controller", allocation_id, "DeleteInstance")
            .expect("begin interrupted cleanup")
            .expect("allocation exists");
        assert!(admission.acquired);
        action_ids.push(admission.action_id.expect("reusable delete action"));
    }
    journal
        .preserve_ambiguous_child_result(
            "startup-woken",
            r#"{"provider_id":"i-startup"}"#,
            "first positive create return",
        )
        .expect("record positive evidence during delete");
    let connection = rusqlite::Connection::open(&database).expect("inspect evidence wake");
    let evidence_due: i64 = connection
        .query_row(
            "SELECT next_attempt_at FROM allocation_actions WHERE allocation_id='startup-woken' AND reusable=1",
            [],
            |row| row.get(0),
        )
        .expect("read evidence due time");
    drop(connection);
    journal
        .recover_startup_allocations()
        .expect("recover interrupted cleanup attempts");
    for (index, allocation_id) in ["startup-normal", "startup-woken"].into_iter().enumerate() {
        let connection = rusqlite::Connection::open(&database).expect("inspect recovered action");
        let action: (i64, i64, i64, String, i64) = connection
            .query_row(
                "SELECT attempt_count, last_attempt_at, next_attempt_at, state, (SELECT COUNT(*) FROM allocation_actions WHERE allocation_id=?1) FROM allocation_actions WHERE allocation_id=?1 AND reusable=1",
                [allocation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .expect("read recovered retry fields");
        assert_eq!(action.0, 1);
        assert_eq!(action.3, "pending");
        assert_eq!(action.4, 1, "startup appends no cleanup action");
        if index == 0 {
            assert_eq!(
                action.2,
                action.1 + 1,
                "ordinary interruption keeps backoff"
            );
        } else {
            assert_eq!(
                action.2, evidence_due,
                "first positive wake survives startup"
            );
        }
        drop(connection);
        assert_eq!(
            journal
                .first_allocation_delete_intent(allocation_id)
                .expect("first intent survives startup"),
            Some(first_intents[index])
        );
        let current = journal
            .allocation_action(allocation_id)
            .expect("read current action")
            .expect("one action exists");
        assert_eq!(current.action_id, action_ids[index]);
    }
    assert_eq!(
        journal
            .active_allocation_counts("controller", "pool")
            .expect("interrupted cleanup remains owned"),
        (2, 2)
    );
    drop(journal);
    std::fs::remove_file(&database).expect("remove test database");
    std::fs::remove_file(database.with_extension("lock")).expect("remove test lock");
}
