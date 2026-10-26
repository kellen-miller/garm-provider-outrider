//! Linux-supported targets retain the exact-name system execution guard.
use crate::{process, root};
use anyhow::{Result, ensure};
use std::process::Command;

const TARGETS: &[&str] = &[
    "core_contract",
    "control_contract",
    "provider_contract",
    "aws_contract",
    "azure_contract",
    "gcp_contract",
    "ingress_contract",
    "mac_contract",
    "allocation_contract",
    "recovery_contract",
    "journal_contract",
    "cleanup_cli",
];

const SYSTEM_CASES: &[&str] = &[
    "authenticated_explain::cli_explain_uses_authenticated_daemon_and_read_only_sdk_analysis",
    "authenticated_explain::cli_explain_matches_cross_cloud_create_order_without_mutations",
    "price_provenance::central_effective_price_changes_cross_cloud_create_winner_and_explains_provenance",
    "explain_deadlines::explain_remote_bounds_chunked_body_and_uses_analysis_deadline",
    "daemon_restart::restart_idempotent_end_to_end",
    "daemon_restart::active_allocation_requires_its_snapshot_fingerprint",
    "daemon_restart::live_allocation_requires_historical_files_and_snapshot",
    "daemon_restart::drained_pool_reconnects_only_live_catalog_scope",
    "daemon_restart::create_waits_for_a_valid_allocation_longer_than_provider_timeout",
    "daemon_restart::repository_registration_labels",
    "cross_cloud_cleanup::unsupported_macos_fail_fast",
    "cross_cloud_cleanup::signed_event_to_cross_cloud_cleanup",
    "cross_cloud_cleanup::tagged_garm_v010_wrapper_process",
];

/// Run every supported Linux target and exactly one test per system filter.
/// # Errors
/// Rejects other hosts, failed tests, or filters executing zero/multiple tests.
pub fn run() -> Result<()> {
    ensure!(
        std::env::consts::OS == "linux",
        "Linux contracts require a Linux host"
    );
    let mut arguments = vec![
        "test",
        "--locked",
        "--workspace",
        "--all-features",
        "--lib",
        "--bins",
    ];
    for target in TARGETS {
        arguments.extend(["--test", target]);
    }

    let mut compilation = arguments.clone();
    compilation.extend(["--test", "system_contract", "--no-run"]);
    process::run(Command::new("cargo").args(&compilation).current_dir(root()))?;
    process::run(Command::new("cargo").args(&arguments).current_dir(root()))?;

    for case in SYSTEM_CASES {
        let output = process::run(
            Command::new("cargo")
                .args([
                    "test",
                    "--locked",
                    "--all-features",
                    "--test",
                    "system_contract",
                    case,
                    "--",
                    "--exact",
                ])
                .current_dir(root()),
        )?;
        validate_exact_result(case, &output)?;
    }

    Ok(())
}

fn validate_exact_result(case: &str, output: &str) -> Result<()> {
    ensure!(
        output
            .lines()
            .filter(|line| line.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"))
            .count()
            == 1,
        "expected exactly one successful test: {case}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use std::{collections::BTreeSet, fs};

    #[test]
    fn every_integration_target_has_explicit_platform_coverage() -> Result<()> {
        let expected: BTreeSet<String> = TARGETS
            .iter()
            .copied()
            .chain([
                "system_contract",
                "admission_contract",
                "bootstrap_contract",
            ])
            .map(str::to_owned)
            .collect();
        let mut actual = BTreeSet::new();
        for entry in fs::read_dir(root().join("tests"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|extension| extension == "rs") {
                actual.insert(
                    path.file_stem()
                        .and_then(|name| name.to_str())
                        .context("integration target must have a UTF-8 name")?
                        .to_owned(),
                );
            }
        }

        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn system_filters_reject_zero_multiple_and_ignored_results() {
        for result in [
            "test result: ok. 0 passed; 0 failed; 0 ignored;",
            "test result: ok. 2 passed; 0 failed; 0 ignored;",
            "test result: ok. 0 passed; 0 failed; 1 ignored;",
        ] {
            assert!(validate_exact_result("case", result).is_err());
        }

        assert!(
            validate_exact_result(
                "case",
                "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 13 filtered out;"
            )
            .is_ok()
        );
    }
}
