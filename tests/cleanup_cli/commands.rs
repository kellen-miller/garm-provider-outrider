use super::cleanup_scenario::AttestedCleanupFixture;
use super::native_scope::read_audited_scope;

use std::fs;

use outrider::{cleanup::CleanupInspection, journal::Journal};

use super::cleanup_scenario::{
    assert_cli_refuses_live_journal_owner, assert_compacted_cleanup_audit, attestation,
    cli_with_provider_metadata, fixture, inspect_for_attestation, invoke_resolve,
};

#[test]
fn actual_cli_inspects_resolves_idempotently_and_preserves_name_fence() {
    let AttestedCleanupFixture { fixture, proof } = inspect_for_attestation("success");
    assert!(fixture.inspection.native_resource_id.is_none());
    assert!(!fixture.inspection.native_identity_known);
    assert!(fixture.inspection.cleanup_attempt_count > 0);
    let journal = Journal::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("open journal for unrelated allocation: {error}"));
    journal
        .reserve_allocation(
            "unrelated-id",
            "other-controller",
            "other-runner",
            "snapshot-1",
            "other-pool",
        )
        .unwrap_or_else(|error| panic!("reserve unrelated allocation: {error}"));
    drop(journal);

    assert_cli_refuses_live_journal_owner(&fixture, &proof);

    let resolved = invoke_resolve(&fixture, &proof);
    assert!(
        resolved.status.success(),
        "{}",
        String::from_utf8_lossy(&resolved.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&resolved.stdout).trim(), "resolved");
    let repeated = invoke_resolve(&fixture, &proof);
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&repeated.stdout).trim(),
        "already resolved"
    );
    let mut conflicting_proof = proof.clone();
    conflicting_proof.evidence_reference = "ticket:INC-43#evidence-2".into();
    assert!(
        !invoke_resolve(&fixture, &conflicting_proof)
            .status
            .success()
    );

    let journal = Journal::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("reopen resolved fixture: {error}"));
    journal
        .recover_startup_allocations()
        .unwrap_or_else(|error| panic!("recover resolved fixture: {error}"));
    assert_eq!(
        journal
            .allocation_by_id("allocation-1")
            .unwrap_or_else(|error| panic!("read resolved allocation: {error}"))
            .map(|allocation| allocation.state),
        Some("deleted".into())
    );
    assert_eq!(
        journal
            .active_allocation_counts("controller-1", "pool-1")
            .unwrap(),
        (0, 0)
    );
    assert!(
        !journal
            .reserve_allocation("new-id", "controller-1", "runner-1", "snapshot-1", "pool-1")
            .unwrap_or_else(|error| panic!("check duplicate name fence: {error}"))
            .1
    );
    let connection = rusqlite::Connection::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("read fixture audit: {error}"));
    let audit_count: u32 = connection
        .query_row("SELECT COUNT(*) FROM cleanup_resolution_audit", [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|error| panic!("count fixture audit: {error}"));
    assert_eq!(audit_count, 1);
    let summary_json: String = connection
        .query_row(
            "SELECT summary_json FROM cleanup_resolution_audit",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|error| panic!("read safe audit summary: {error}"));
    assert!(summary_json.contains("native_account_id"));
    assert!(summary_json.contains("native_location"));
    assert!(!summary_json.contains("/private/credential-file"));
    assert_eq!(
        journal
            .allocation("other-controller", "other-runner")
            .unwrap_or_else(|error| panic!("read unrelated allocation: {error}"))
            .map(|record| record.state),
        Some("no_candidate".into())
    );
    drop(journal);
    drop(connection);
    assert_compacted_cleanup_audit(&fixture, &proof, &summary_json);
    assert!(
        !outrider::cleanup::resolve(
            &Journal::open(&fixture.temp.path)
                .unwrap_or_else(|error| panic!("reopen terminal cleanup journal: {error}")),
            &proof,
            proof.verified_at_unix_seconds.saturating_add(3601)
        )
        .unwrap_or_else(|error| panic!("repeat expired identical proof: {error}"))
    );
}

#[test]
fn explicit_cleanup_commands_take_precedence_over_inherited_provider_metadata() {
    let fixture = fixture("provider-metadata-routing");
    let inspection_output = cli_with_provider_metadata(&[
        "cleanup",
        "inspect",
        "--database",
        fixture.temp.path.to_str().unwrap_or_default(),
        "--controller",
        "controller-1",
        "--allocation",
        "allocation-1",
    ]);
    assert!(
        inspection_output.status.success(),
        "{}",
        String::from_utf8_lossy(&inspection_output.stderr)
    );
    let inspection: CleanupInspection = serde_json::from_slice(&inspection_output.stdout)
        .unwrap_or_else(|error| panic!("parse routed cleanup inspection: {error}"));
    assert_eq!(inspection.allocation_id, "allocation-1");
    assert_eq!(inspection.controller_id, "controller-1");
    assert_eq!(inspection.allocation_state, "deleting");

    let proof = attestation(&inspection);
    let proof_path = fixture.temp.path.with_extension("routing-attestation.json");
    fs::write(
        &proof_path,
        serde_json::to_vec(&proof)
            .unwrap_or_else(|error| panic!("serialize routed attestation: {error}")),
    )
    .unwrap_or_else(|error| panic!("write routed attestation: {error}"));
    let resolution_output = cli_with_provider_metadata(&[
        "cleanup",
        "resolve",
        "--database",
        fixture.temp.path.to_str().unwrap_or_default(),
        "--attestation",
        proof_path.to_str().unwrap_or_default(),
    ]);
    fs::remove_file(proof_path)
        .unwrap_or_else(|error| panic!("remove routed attestation: {error}"));
    assert!(
        resolution_output.status.success(),
        "{}",
        String::from_utf8_lossy(&resolution_output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&resolution_output.stdout).trim(),
        "resolved"
    );

    let journal = Journal::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("reopen routed cleanup journal: {error}"));
    assert_eq!(
        journal
            .allocation_by_id("allocation-1")
            .unwrap_or_else(|error| panic!("read routed allocation: {error}"))
            .map(|allocation| allocation.state),
        Some("deleted".into())
    );
    let audit = read_audited_scope(&fixture);
    assert_eq!(audit["allocation_id"], "allocation-1");
    assert_eq!(audit["scope_digest"], proof.scope_digest);
    let connection = rusqlite::Connection::open(&fixture.temp.path)
        .unwrap_or_else(|error| panic!("read routed cleanup audit: {error}"));
    let audit_count: u32 = connection
        .query_row("SELECT COUNT(*) FROM cleanup_resolution_audit", [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|error| panic!("count routed cleanup audit: {error}"));
    assert_eq!(audit_count, 1);
}
