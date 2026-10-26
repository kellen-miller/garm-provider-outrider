//! Repository operations and native test-provider installation.
pub mod acceptance;
pub mod bundle;
pub mod contract_tools;
pub mod linux_contracts;
pub mod package;
pub mod process;
pub mod provider_fixture;

/// Repository source root used by development operations.
#[must_use]
pub fn root() -> &'static std::path::Path {
    std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
}
