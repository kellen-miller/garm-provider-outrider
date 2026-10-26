use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt, fs::PermissionsExt};

use serde::Deserialize;
use thiserror::Error;

/// Selected native registration, without credentials or environment values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeProviderRegistration {
    pub name: String,
    pub executable_path: PathBuf,
    pub configuration_path: PathBuf,
    pub inherited_environment: Vec<String>,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativeGarmError {
    #[error("native GARM configuration could not be read")]
    ConfigurationRead,
    #[error("native GARM configuration is invalid")]
    InvalidConfiguration,
    #[error("native GARM provider names are duplicated")]
    DuplicateProvider,
    #[error("selected native GARM provider names are duplicated")]
    DuplicateSelection,
    #[error("selected native GARM provider is missing")]
    UnknownProvider,
    #[error("selected native GARM provider type is unsupported")]
    UnsupportedProviderType,
    #[error("selected native GARM interface is unsupported")]
    UnsupportedInterfaceVersion,
    #[error("selected native GARM executable is invalid")]
    InvalidExecutable,
    #[error("selected native GARM provider configuration is invalid")]
    InvalidProviderConfiguration,
    #[error("native GARM provider would recurse into Outrider")]
    RecursiveProvider,
    #[error("selected native GARM environment name is invalid")]
    InvalidEnvironment,
    #[error("native GARM imports require a Unix host")]
    UnsupportedHost,
}

#[derive(Deserialize)]
struct GarmProviders {
    #[serde(default)]
    provider: Vec<GarmProvider>,
}

#[derive(Deserialize)]
struct GarmProvider {
    name: String,
    #[serde(default)]
    provider_type: String,
    #[serde(default)]
    external: GarmExternal,
}

#[derive(Default, Deserialize)]
struct GarmExternal {
    #[serde(default)]
    interface_version: String,
    #[serde(default)]
    provider_executable: PathBuf,
    #[serde(default)]
    provider_dir: PathBuf,
    #[serde(default)]
    config_file: PathBuf,
    #[serde(default)]
    environment_variables: Vec<String>,
}

/// Read selected native GARM registrations and resolve their startup environment names.
///
/// Paths follow GARM v0.2.1: absolute, with `provider_executable` taking precedence
/// over `provider_dir/garm-external-provider`. Environment selectors are prefixes;
/// values remain exclusively in the caller's immutable environment snapshot.
///
/// # Errors
/// Returns fixed errors for malformed or unsupported selected registrations,
/// inaccessible files, recursive Outrider execution, and unrepresentable names.
pub fn load_native_garm_providers(
    garm_config_path: &Path,
    selected_provider_names: &[String],
    outrider_provider_name: &str,
    environment_snapshot: &BTreeMap<OsString, OsString>,
) -> Result<BTreeMap<String, NativeProviderRegistration>, NativeGarmError> {
    let contents = std::fs::read_to_string(garm_config_path)
        .map_err(|_| NativeGarmError::ConfigurationRead)?;
    let document: GarmProviders =
        toml_edit::de::from_str(&contents).map_err(|_| NativeGarmError::InvalidConfiguration)?;
    let mut providers = BTreeMap::new();
    for provider in document.provider {
        if provider.name.is_empty() {
            return Err(NativeGarmError::InvalidConfiguration);
        }

        if providers.insert(provider.name.clone(), provider).is_some() {
            return Err(NativeGarmError::DuplicateProvider);
        }
    }

    let mut selected = BTreeMap::new();
    for name in selected_provider_names {
        if selected.contains_key(name) {
            return Err(NativeGarmError::DuplicateSelection);
        }

        if name == outrider_provider_name {
            return Err(NativeGarmError::RecursiveProvider);
        }

        let provider = providers
            .get(name)
            .ok_or(NativeGarmError::UnknownProvider)?;
        if provider.provider_type != "external" {
            return Err(NativeGarmError::UnsupportedProviderType);
        }

        if !provider.external.interface_version.is_empty()
            && provider.external.interface_version != super::GARM_INTERFACE_VERSION
        {
            return Err(NativeGarmError::UnsupportedInterfaceVersion);
        }

        let external = &provider.external;
        let executable_path = if external.provider_executable.as_os_str().is_empty() {
            external.provider_dir.join("garm-external-provider")
        } else {
            external.provider_executable.clone()
        };
        validate_native_files(&executable_path, &external.config_file)?;
        let inherited_environment =
            resolve_environment_names(&external.environment_variables, environment_snapshot)?;
        selected.insert(
            name.clone(),
            NativeProviderRegistration {
                name: name.clone(),
                executable_path,
                configuration_path: external.config_file.clone(),
                inherited_environment,
            },
        );
    }

    Ok(selected)
}

fn validate_native_files(executable: &Path, configuration: &Path) -> Result<(), NativeGarmError> {
    if !executable.is_absolute() {
        return Err(NativeGarmError::InvalidExecutable);
    }

    if !configuration.is_absolute() {
        return Err(NativeGarmError::InvalidProviderConfiguration);
    }

    let executable_metadata =
        std::fs::metadata(executable).map_err(|_| NativeGarmError::InvalidExecutable)?;
    let configuration_metadata = std::fs::metadata(configuration)
        .map_err(|_| NativeGarmError::InvalidProviderConfiguration)?;
    if !configuration_metadata.is_file() || configuration_metadata.len() == 0 {
        return Err(NativeGarmError::InvalidProviderConfiguration);
    }

    if !executable_metadata.is_file() {
        return Err(NativeGarmError::InvalidExecutable);
    }

    #[cfg(unix)]
    {
        let own_executable =
            std::env::current_exe().map_err(|_| NativeGarmError::InvalidExecutable)?;
        let own_metadata =
            std::fs::metadata(own_executable).map_err(|_| NativeGarmError::InvalidExecutable)?;
        if executable_metadata.dev() == own_metadata.dev()
            && executable_metadata.ino() == own_metadata.ino()
        {
            return Err(NativeGarmError::RecursiveProvider);
        }

        if executable_metadata.permissions().mode() & 0o111 == 0 {
            return Err(NativeGarmError::InvalidExecutable);
        }

        Ok(())
    }

    #[cfg(not(unix))]
    Err(NativeGarmError::UnsupportedHost)
}

fn resolve_environment_names(
    prefixes: &[String],
    environment: &BTreeMap<OsString, OsString>,
) -> Result<Vec<String>, NativeGarmError> {
    let mut names = BTreeSet::new();
    for name in environment.keys() {
        #[cfg(unix)]
        let bytes = name.as_os_str().as_bytes();
        #[cfg(not(unix))]
        let bytes = name.as_encoded_bytes();
        if bytes.starts_with(b"GARM")
            || !prefixes
                .iter()
                .any(|prefix| bytes.starts_with(prefix.as_bytes()))
        {
            continue;
        }

        let name = name.to_str().ok_or(NativeGarmError::InvalidEnvironment)?;
        if name.is_empty() || name.bytes().any(|byte| byte == b'=' || byte == 0) {
            return Err(NativeGarmError::InvalidEnvironment);
        }

        names.insert(name.to_owned());
    }

    Ok(names.into_iter().collect())
}

#[cfg(all(test, unix))]
#[path = "native_garm/tests.rs"]
mod tests;
