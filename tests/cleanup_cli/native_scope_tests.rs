use super::native_scope::NativeCloudScope;

use outrider::cleanup::CleanupInspection;

use super::cleanup_scenario::{fixture, invoke_inspect};
use super::native_scope::assert_native_cli_resolution;
#[test]
fn azure_and_gcp_cli_resolve_source_correct_native_scopes() {
    assert_native_cli_resolution(
        "azure-native",
        NativeCloudScope::Azure,
        "logical-azure-account",
        "subscription-Native-123",
    );
    assert_native_cli_resolution(
        "gcp-native",
        NativeCloudScope::Gcp,
        "project-native-456",
        "project-native-456",
    );
    assert_native_cli_resolution(
        "gcp-bare-image",
        NativeCloudScope::GcpBareImage,
        "project-native-456",
        "project-native-456",
    );
}

#[test]
fn cleanup_inspect_reports_the_reusable_delete_across_legacy_rows() {
    let fixture = fixture("legacy-delete-rows");
    let connection = rusqlite::Connection::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("open legacy delete fixture: {error}"));
    let (reusable_id, first_intent_at): (i64, i64) = connection
        .query_row(
            "SELECT action_id, started_at FROM allocation_actions WHERE allocation_id='allocation-1' AND command='DeleteInstance' AND reusable=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or_else(|error| panic!("read reusable delete intent: {error}"));
    connection
        .execute(
            "UPDATE allocation_actions SET attempt_count=4, last_attempt_at=100, next_attempt_at=900 WHERE action_id=?1",
            [reusable_id],
        )
        .unwrap_or_else(|error| panic!("set reusable delete retry state: {error}"));
    for (started_at, attempt_count, next_attempt_at) in
        [(1000_i64, 88_i64, 2000_i64), (1100, 99, 3000)]
    {
        connection
            .execute(
                "INSERT INTO allocation_actions(allocation_id, command, state, started_at, detail, unresolved_create, reusable, attempt_count, last_attempt_at, next_attempt_at) VALUES ('allocation-1', 'DeleteInstance', 'ambiguous', ?1, 'legacy row', 0, 0, ?2, ?1, ?3)",
                rusqlite::params![started_at, attempt_count, next_attempt_at],
            )
            .unwrap_or_else(|error| panic!("insert legacy delete row: {error}"));
    }
    drop(connection);

    let output = invoke_inspect(&fixture);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let inspection: CleanupInspection = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("parse legacy cleanup inspection: {error}"));
    assert_eq!(inspection.cleanup_action_id, reusable_id);
    assert_eq!(inspection.cleanup_attempt_count, 4);
    assert_eq!(inspection.cleanup_last_attempt_at, Some(100));
    assert_eq!(inspection.cleanup_next_attempt_at, Some(900));
    let current = inspection
        .unresolved_actions
        .iter()
        .find(|action| action.action_id == reusable_id)
        .unwrap_or_else(|| panic!("show reusable first delete intent"));
    assert!(current.reusable);
    assert_eq!(
        current.started_at,
        u64::try_from(first_intent_at).unwrap_or_default()
    );
    let legacy_rows = inspection
        .unresolved_actions
        .iter()
        .filter(|action| action.action_id != reusable_id)
        .collect::<Vec<_>>();
    assert_eq!(legacy_rows.len(), 2);
    assert!(legacy_rows.iter().all(|action| !action.reusable));
}
