//! Native provider-process fixtures shared by integration scenarios.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    #[default]
    Protocol,
    Recovery,
    System,
}

#[derive(Default, Deserialize, Serialize)]
pub struct Configuration {
    pub scenario: Scenario,
    pub native_locator: Option<String>,
    pub lifecycle_log: Option<PathBuf>,
}

/// Build the lightweight fixture once per integration-test process.
/// # Panics
/// Panics when Cargo cannot produce the required fixture executable.
#[must_use]
pub fn build_executable() -> PathBuf {
    static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
    EXECUTABLE
        .get_or_init(|| {
            let current =
                std::env::current_exe().unwrap_or_else(|_| panic!("integration executable"));
            let profile_directory = current
                .parent()
                .and_then(Path::parent)
                .unwrap_or_else(|| panic!("Cargo profile directory"));
            let target = profile_directory
                .parent()
                .unwrap_or_else(|| panic!("Cargo target directory"));
            let profile = profile_directory
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or_else(|| panic!("Cargo profile name"));
            let profile = if profile == "debug" { "dev" } else { profile };
            let status = Command::new("cargo")
                .args([
                    "build",
                    "--locked",
                    "--package",
                    "outrider-xtask",
                    "--bin",
                    "outrider-provider-fixture",
                    "--profile",
                    profile,
                    "--target-dir",
                ])
                .arg(target)
                .current_dir(crate::root())
                .status()
                .unwrap_or_else(|_| panic!("build native provider fixture"));
            assert!(
                status.success(),
                "native provider fixture compilation failed"
            );
            profile_directory.join(format!(
                "outrider-provider-fixture{}",
                std::env::consts::EXE_SUFFIX
            ))
        })
        .clone()
}

/// Copy a native fixture and its scenario-owned configuration.
/// # Errors
/// Returns file or serialization failures.
pub fn install(destination: &Path, configuration: &Configuration) -> std::io::Result<()> {
    fs::copy(build_executable(), destination)?;
    fs::write(
        destination.with_extension("json"),
        serde_json::to_vec(configuration)?,
    )
}
