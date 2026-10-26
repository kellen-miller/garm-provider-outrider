//! Installation qualification owns generated pins; native provider configuration stays in GARM.

use crate::{
    config::CentralConfig, installation::inspect_package, providers::load_native_garm_providers,
};
use clap::Args;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
use thiserror::Error;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Table, value};

pub(crate) mod adoption;
mod configuration;
mod doctor;
use configuration::ConfigurationFile;
pub use doctor::{DoctorOptions, DoctorReport, doctor};

#[derive(Args)]
pub struct SetupOptions {
    #[arg(long, default_value = "/etc/outrider/config.toml")]
    pub config: PathBuf,
    #[arg(long, default_value = "/opt/outrider")]
    pub package: PathBuf,
    #[arg(long, default_value = "default")]
    pub profile: String,
    /// Add only Outrider's own entry to the existing native GARM registry.
    #[arg(long)]
    pub register: bool,
}

#[derive(Debug, Error)]
pub enum SetupError {
    #[error(
        "configuration is incomplete or invalid; supply operator facts and protected credential references"
    )]
    Configuration,
    #[error(
        "configuration requires an absolute protected regular file and trusted non-writable ancestors"
    )]
    ProtectedConfiguration,
    #[error("configuration changed during setup; no concurrent update is permitted")]
    ConcurrentEdit,
    #[error(
        "package is invalid or does not match the selected native provider binary; install the qualified provider without replacing its native configuration"
    )]
    Package,
    #[error(
        "existing qualification pins differ; retain that revision for active allocations and prepare a new configuration revision"
    )]
    PinsChanged,
    #[error(
        "Outrider's native registration already exists with different settings; existing entries were preserved"
    )]
    Registration,
    #[error(
        "API identity or permissions could not be verified; check GARM/GitHub origins, credentials and repository linkage"
    )]
    Identity,
    #[error("online doctor requires the running daemon and an eligible live-priced candidate")]
    LiveSelection,
    #[error(
        "daemon loaded a different configuration revision; restart it before running online doctor"
    )]
    StaleDaemon,
    #[error(transparent)]
    Authentication(#[from] crate::authentication::AuthError),
}

#[derive(Serialize)]
pub struct SetupReport {
    pub configuration: PathBuf,
    pub providers_qualified: usize,
    pub api_identities_verified: bool,
    pub registration_written: bool,
    pub registration_toml: String,
    pub next_step: &'static str,
}

/// Qualify local artifacts and adopt existing API identities without creating cloud resources.
///
/// # Errors
/// Rejects unsafe files, changed pins, unsupported packages or unverified existing identities.
pub fn setup(options: &SetupOptions) -> Result<SetupReport, SetupError> {
    let package = inspect_package(&options.package).map_err(|_| SetupError::Package)?;
    let configuration = ConfigurationFile::open(&options.config)?;
    let mut document = configuration
        .text()?
        .parse::<DocumentMut>()
        .map_err(|_| SetupError::Configuration)?;
    let source =
        CentralConfig::from_toml(&document.to_string()).map_err(|_| SetupError::Configuration)?;
    let garm_path = source
        .garm
        .as_ref()
        .ok_or(SetupError::Configuration)?
        .config_file
        .clone();
    let ingress = source.ingress.as_ref().ok_or(SetupError::Configuration)?;
    let names = source
        .providers
        .iter()
        .map(|provider| provider.name.clone())
        .collect::<Vec<_>>();
    if names.is_empty() {
        return Err(SetupError::Configuration);
    }

    // Retain a protected handle before the native loader reads registry entries.
    let garm_file = ConfigurationFile::open(&garm_path)?;
    let mut garm_document = garm_file
        .text()?
        .parse::<DocumentMut>()
        .map_err(|_| SetupError::Configuration)?;
    if ingress.garm_api_origin.is_empty() {
        document["ingress"]["garm_api_origin"] = value(native_api_origin(&garm_document)?);
    }

    let registrations = load_native_garm_providers(
        &garm_path,
        &names,
        &ingress.provider_name,
        &std::env::vars_os().collect::<BTreeMap<_, _>>(),
    )
    .map_err(|_| SetupError::Configuration)?;
    for provider in document["providers"]
        .as_array_of_tables_mut()
        .ok_or(SetupError::Configuration)?
        .iter_mut()
    {
        let name = provider["name"].as_str().ok_or(SetupError::Configuration)?;
        let cloud = provider["cloud"]
            .as_str()
            .ok_or(SetupError::Configuration)?;
        let native = registrations.get(name).ok_or(SetupError::Configuration)?;
        let pin = package
            .providers
            .iter()
            .find(|pin| pin.cloud == cloud)
            .ok_or(SetupError::Package)?;
        if artifact_digest(&native.executable_path)? != pin.binary_sha256 {
            return Err(SetupError::Package);
        }

        let configuration_sha256 = artifact_digest(&native.configuration_path)?;
        for (field, expected) in [
            ("executable_sha256", pin.binary_sha256.as_str()),
            ("configuration_sha256", configuration_sha256.as_str()),
            ("source_revision", pin.source_revision.as_str()),
            ("expected_version", pin.expected_version.as_str()),
        ] {
            if let Some(previous) = provider.get(field).and_then(toml_edit::Item::as_str)
                && !previous.is_empty()
                && !previous.bytes().all(|byte| byte == b'0')
                && previous != expected
            {
                return Err(SetupError::PinsChanged);
            }

            provider[field] = value(expected);
        }

        for (field, defaults) in [
            ("supported_os", vec!["linux"]),
            ("supported_architectures", vec!["amd64"]),
            ("purchasing_capabilities", vec!["on_demand", "spot"]),
        ] {
            if provider.get(field).is_none() {
                provider[field] = value(defaults.into_iter().collect::<Array>());
            }
        }
    }

    let source =
        CentralConfig::from_toml(&document.to_string()).map_err(|_| SetupError::Configuration)?;
    let own_name = source
        .ingress
        .as_ref()
        .ok_or(SetupError::Configuration)?
        .provider_name
        .clone();
    source
        .load_native_configuration(&std::env::vars_os().collect())
        .and_then(|config| config.normalize_profile(None, &options.profile, None))
        .map_err(|_| SetupError::Configuration)?;
    let registration_toml = registration(&own_name, &options.config, &options.package)?;
    if options.register {
        merge_registration(&mut garm_document, &own_name, &registration_toml)?;
    }

    // Both documents are fully checked before the first write. They are individually atomic;
    // registration failure may leave qualified config, and an unchanged rerun safely retries.
    configuration.publish(document.to_string().as_bytes())?;
    let registration_written =
        options.register && garm_file.publish(garm_document.to_string().as_bytes())?;
    Ok(SetupReport {
        configuration: options.config.clone(),
        providers_qualified: names.len(),
        api_identities_verified: false,
        registration_written,
        registration_toml,
        next_step: "start Outrider for automatic authentication/adoption, run doctor --online, then route signed webhooks",
    })
}

fn artifact_digest(path: &Path) -> Result<String, SetupError> {
    let mut file = File::open(path).map_err(|_| SetupError::ProtectedConfiguration)?;
    let metadata = file
        .metadata()
        .map_err(|_| SetupError::ProtectedConfiguration)?;
    if !metadata.is_file() || metadata.len() > 512 * 1024 * 1024 {
        return Err(SetupError::ProtectedConfiguration);
    }

    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| SetupError::ProtectedConfiguration)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }

    Ok(hex::encode(digest.finalize()))
}

fn native_api_origin(document: &DocumentMut) -> Result<String, SetupError> {
    let api = document.get("apiserver").ok_or(SetupError::Configuration)?;
    let bind = api
        .get("bind")
        .and_then(toml_edit::Item::as_str)
        .ok_or(SetupError::Configuration)?;
    let mut address = bind
        .parse::<std::net::IpAddr>()
        .map_err(|_| SetupError::Configuration)?;
    if address.is_unspecified() {
        address = if address.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        };
    }

    let port = api
        .get("port")
        .and_then(toml_edit::Item::as_integer)
        .and_then(|value| u16::try_from(value).ok())
        .filter(|port| *port > 0)
        .ok_or(SetupError::Configuration)?;
    let scheme = if api
        .get("use_tls")
        .and_then(toml_edit::Item::as_bool)
        .unwrap_or(false)
    {
        "https"
    } else {
        "http"
    };
    Ok(format!(
        "{scheme}://{}/",
        std::net::SocketAddr::new(address, port)
    ))
}

fn registration(name: &str, config: &Path, package: &Path) -> Result<String, SetupError> {
    let mut provider = Table::new();
    provider["name"] = value(name);
    provider["provider_type"] = value("external");
    provider["disable_jit_config"] = value(true);
    let mut external = Table::new();
    external["interface_version"] = value("v0.1.0");
    external["provider_executable"] = value(
        package
            .join("bin/outrider")
            .to_str()
            .ok_or(SetupError::Configuration)?,
    );
    external["config_file"] = value(config.to_str().ok_or(SetupError::Configuration)?);
    external["environment_variables"] = value(Array::new());
    provider["external"] = toml_edit::Item::Table(external);
    let mut providers = ArrayOfTables::new();
    providers.push(provider);
    let mut document = DocumentMut::new();
    document["provider"] = toml_edit::Item::ArrayOfTables(providers);
    Ok(document.to_string())
}

fn merge_registration(
    document: &mut DocumentMut,
    name: &str,
    registration: &str,
) -> Result<(), SetupError> {
    let mut incoming = registration
        .parse::<DocumentMut>()
        .map_err(|_| SetupError::Configuration)?;
    let expected = incoming["provider"]
        .as_array_of_tables_mut()
        .and_then(|providers| providers.get(0))
        .ok_or(SetupError::Configuration)?;
    if document.get("provider").is_none() {
        document["provider"] = toml_edit::Item::ArrayOfTables(ArrayOfTables::new());
    }

    let providers = document["provider"]
        .as_array_of_tables_mut()
        .ok_or(SetupError::Configuration)?;
    let matches = providers
        .iter()
        .filter(|provider| provider["name"].as_str() == Some(name))
        .collect::<Vec<_>>();
    if !matches.is_empty() {
        if matches.len() != 1
            || matches[0]["provider_type"].as_str() != expected["provider_type"].as_str()
            || matches[0]["disable_jit_config"].as_bool()
                != expected["disable_jit_config"].as_bool()
            || ["interface_version", "provider_executable", "config_file"]
                .iter()
                .any(|field| {
                    matches[0]["external"][field].as_str() != expected["external"][field].as_str()
                })
        {
            return Err(SetupError::Registration);
        }

        return Ok(());
    }

    providers.push(expected.clone());
    Ok(())
}

#[cfg(test)]
mod tests;
