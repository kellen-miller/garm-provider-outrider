#![cfg(unix)]

use outrider::journal::{Journal, JournalError};
use serde_json::json;

use super::journal_process::journal_path;

#[test]
fn inventory_ignores_local_selection_but_retains_dispatch_history_after_deletion() {
    let path = journal_path();
    let journal = Journal::open(&path).unwrap();
    let initial = journal.inventory_snapshot("controller").unwrap();
    for id in ["reserved", "selecting", "no_candidate"] {
        journal
            .reserve_allocation(id, "controller", id, "snapshot", "pool")
            .unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE allocations SET state=?1 WHERE allocation_id=?1",
                [id],
            )
            .unwrap();
    }

    let local = journal.inventory_snapshot("controller").unwrap();
    assert!(local.active_allocations.is_empty());
    assert_eq!(local.dispatch_fingerprint, initial.dispatch_fingerprint);
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute("UPDATE allocations SET state='dispatched',dispatched_at=100 WHERE allocation_id='reserved'",[]).unwrap();
    let dispatched = journal.inventory_snapshot("controller").unwrap();
    assert_eq!(dispatched.active_allocations.len(), 1);
    assert_ne!(
        dispatched.dispatch_fingerprint,
        initial.dispatch_fingerprint
    );
    connection
        .execute(
            "UPDATE allocations SET state='deleted' WHERE allocation_id='reserved'",
            [],
        )
        .unwrap();
    let deleted = journal.inventory_snapshot("controller").unwrap();
    assert!(deleted.active_allocations.is_empty());
    assert_eq!(
        deleted.dispatch_fingerprint,
        dispatched.dispatch_fingerprint
    );
}

#[test]
fn inventory_projects_native_identity_without_private_provider_or_dependency_values() {
    let path = journal_path();
    let journal = Journal::open(&path).unwrap();
    journal
        .reserve_allocation("active", "controller", "runner", "snapshot", "pool")
        .unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    let identity = json!({"controller_id":"controller","cloud":"azure","account_id":"subscription","location":"eastus","resource_id":"/subscriptions/subscription/resourceGroups/runner/providers/Microsoft.Compute/virtualMachines/vm","host_id":null,"dependency_ids":{"vm_id":"native-guid","credential":"must-not-export"}});
    connection.execute("UPDATE allocations SET state='created',dispatched_at=100,native_identity_json=?1,binding_json=?2,provider_instance_json=?2 WHERE allocation_id='active'",rusqlite::params![identity.to_string(),json!({"credential":"must-not-export"}).to_string()]).unwrap();
    let snapshot = journal.inventory_snapshot("controller").unwrap();
    let projected = snapshot.active_allocations[0].identity.as_ref().unwrap();
    assert_eq!(projected.scope.location, "subscription");
    assert_eq!(projected.incarnation.as_deref(), Some("native-guid"));
    let json = serde_json::to_string(&snapshot).unwrap();
    assert!(!json.contains("must-not-export"));
    assert!(!json.contains("eastus"));
    assert!(
        journal
            .inventory_snapshot("other-controller")
            .unwrap()
            .active_allocations
            .is_empty()
    );
}

#[test]
fn inventory_gcp_identity_uses_native_path_and_exact_numeric_incarnation() {
    let path = journal_path();
    let journal = Journal::open(&path).unwrap();
    journal
        .reserve_allocation("active", "controller", "runner", "snapshot", "pool")
        .unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    let identity = json!({"controller_id":"controller","cloud":"gcp","account_id":"project","location":"us-east1-b","resource_id":"18446744073709551615","host_id":null,"dependency_ids":{"instance_name":"runner-native","network":"must-not-export"}});
    connection.execute("UPDATE allocations SET state='created',dispatched_at=100,native_identity_json=?1 WHERE allocation_id='active'",[identity.to_string()]).unwrap();
    let snapshot = journal.inventory_snapshot("controller").unwrap();
    let projected = snapshot.active_allocations[0].identity.as_ref().unwrap();
    assert_eq!(
        projected.resource_id,
        "projects/project/zones/us-east1-b/instances/runner-native"
    );
    assert_eq!(
        projected.incarnation.as_deref(),
        Some("18446744073709551615")
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("must-not-export")
    );
}

#[test]
fn inventory_rejects_unknown_state_or_foreign_native_controller() {
    let path = journal_path();
    let journal = Journal::open(&path).unwrap();
    journal
        .reserve_allocation("active", "controller", "runner", "snapshot", "pool")
        .unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE allocations SET state='future_state' WHERE allocation_id='active'",
            [],
        )
        .unwrap();
    assert!(matches!(
        journal.inventory_snapshot("controller"),
        Err(JournalError::InvalidInventory)
    ));
    let identity = json!({"controller_id":"foreign-controller","cloud":"aws","account_id":"account","location":"us-east-1","resource_id":"i-abc123","host_id":null,"dependency_ids":{}});
    connection.execute("UPDATE allocations SET state='created',dispatched_at=100,native_identity_json=?1 WHERE allocation_id='active'",[identity.to_string()]).unwrap();
    assert!(matches!(
        journal.inventory_snapshot("controller"),
        Err(JournalError::InvalidInventory)
    ));
    assert!(matches!(
        journal.inventory_snapshot(""),
        Err(JournalError::InvalidInventory)
    ));
}

#[test]
fn inventory_cap_does_not_return_truncated_success() {
    let path = journal_path();
    let journal = Journal::open(&path).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("WITH RECURSIVE rows(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM rows WHERE x<10001) INSERT INTO allocations(allocation_id,controller_id,runner_name,snapshot_id,pool_id,state,created_at,dispatched_at) SELECT 'allocation-'||x,'controller','runner-'||x,'snapshot','pool','dispatched',1,1 FROM rows").unwrap();
    assert!(matches!(
        journal.inventory_snapshot("controller"),
        Err(JournalError::InvalidInventory)
    ));
}
