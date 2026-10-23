//! Required-case qualification owns one full test execution and doctests.
use crate::{process, root};
use anyhow::{Context, Result, ensure};
use regex::Regex;
use serde::Deserialize;
use std::{collections::BTreeMap, fs, process::Command};

#[derive(Deserialize)]
struct Manifest {
    milestones: Vec<Milestone>,
}

#[derive(Deserialize)]
struct Milestone {
    id: String,
    target: String,
    required_tests: Vec<String>,
}

/// Run required declarations and outcomes against the retained manifest.
/// # Errors
/// Rejects missing, ignored, duplicate, or failed required cases and failed tools.
pub fn run(selected: Option<&str>) -> Result<()> {
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(root().join("tests/acceptance/cases.json"))?)?;
    let milestones: Vec<&Milestone> = manifest
        .milestones
        .iter()
        .filter(|m| selected.is_none_or(|id| m.id == id))
        .collect();
    ensure!(!milestones.is_empty(), "unknown milestone");
    if selected.is_none() {
        cargo(&[
            "test",
            "--locked",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--no-run",
        ])?;
    }

    for milestone in &milestones {
        let listing = cargo(&[
            "test",
            "--locked",
            "--all-features",
            "--test",
            &milestone.target,
            "--",
            "--list",
        ])?;
        validate_declarations(&milestone.required_tests, &listing)?;
    }

    let arguments = if let Some(milestone) = selected.and_then(|_| milestones.first()) {
        vec![
            "test",
            "--locked",
            "--all-features",
            "--test",
            &milestone.target,
        ]
    } else {
        vec![
            "test",
            "--locked",
            "--workspace",
            "--all-targets",
            "--all-features",
        ]
    };
    let output = cargo(&arguments)?;
    for milestone in &milestones {
        validate_outcomes(&milestone.target, &milestone.required_tests, &output)?;
    }

    if selected.is_none() {
        cargo(&["test", "--locked", "--workspace", "--all-features", "--doc"])?;
    }

    println!(
        "acceptance passed: {} required cases",
        milestones
            .iter()
            .map(|m| m.required_tests.len())
            .sum::<usize>()
    );
    Ok(())
}

fn cargo(arguments: &[&str]) -> Result<String> {
    process::run(
        Command::new("cargo")
            .args(arguments)
            .current_dir(root())
            .env("CARGO_BUILD_JOBS", "2")
            .env("CARGO_TERM_COLOR", "never"),
    )
}

fn validate_declarations(required: &[String], output: &str) -> Result<()> {
    for name in required {
        let matches: Vec<&str> = output
            .lines()
            .filter(|line| line.starts_with(&format!("{name}: test")))
            .collect();
        ensure!(
            matches.len() == 1,
            "{name}: expected one required declaration, found {}",
            matches.len()
        );
        ensure!(
            !matches[0].contains("(ignored)"),
            "required test ignored: {name}"
        );
    }

    Ok(())
}

fn validate_outcomes(target: &str, required: &[String], output: &str) -> Result<()> {
    let executable = Regex::new(r"^\s*Running tests/([^/. ]+)\.rs \(")?;
    let outcome = Regex::new(r"^test ([^ ]+) \.\.\. (ok|FAILED|ignored)(?: .*)?$")?;
    let mut current = false;
    let mut results: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in output.lines() {
        if line.trim_start().starts_with("Running ") {
            current = executable.captures(line).is_some_and(|c| &c[1] == target);
        }

        if current && let Some(capture) = outcome.captures(line) {
            results
                .entry(capture[1].to_owned())
                .or_default()
                .push(capture[2].to_owned());
        }
    }

    for name in required {
        let results = results
            .get(name)
            .context(format!("missing required outcome: {target}::{name}"))?;
        ensure!(
            results.len() == 1 && results[0] == "ok",
            "{target}::{name}: expected one successful outcome, found {results:?}"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_outcomes_reject_missing_duplicate_ignored_failed_and_wrong_target() {
        let names = vec!["required_case".to_owned()];
        for result in [
            "",
            "test required_case ... ignored\n",
            "test required_case ... FAILED\n",
            "test required_case ... ok\ntest required_case ... ok\n",
        ] {
            let output =
                format!("Running tests/core_contract.rs (target/debug/deps/core)\n{result}");
            assert!(validate_outcomes("core_contract", &names, &output).is_err());
        }

        let valid =
            "Running tests/core_contract.rs (target/debug/deps/core)\ntest required_case ... ok\n";
        assert!(validate_outcomes("core_contract", &names, valid).is_ok());
        assert!(validate_outcomes("other_contract", &names, valid).is_err());
    }

    #[test]
    fn declarations_reject_missing_duplicate_and_ignored_cases() {
        let names = vec!["required_case".to_owned()];
        for listing in [
            "",
            "required_case: test (ignored)\n",
            "required_case: test\nrequired_case: test\n",
        ] {
            assert!(validate_declarations(&names, listing).is_err());
        }

        assert!(validate_declarations(&names, "required_case: test\n").is_ok());
    }
}
