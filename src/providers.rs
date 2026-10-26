#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::AsyncReadExt,
    time::{Instant, timeout_at},
};

mod native_garm;
mod process;
pub use native_garm::{NativeGarmError, NativeProviderRegistration, load_native_garm_providers};
use process::run_process;

use crate::garm::{ProviderInstance, ProviderRequest};

pub const GARM_INTERFACE_VERSION: &str = "v0.1.0";

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PurchasingCapability {
    OnDemand,
    Spot,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderBinding {
    pub id: String,
    pub cloud: String,
    pub account: String,
    pub location: String,
    pub executable_path: PathBuf,
    pub executable_sha256: String,
    pub configuration_path: PathBuf,
    pub configuration_sha256: String,
    pub source_revision: String,
    pub expected_version: String,
    pub inherited_environment: Vec<String>,
    pub supported_os: BTreeSet<String>,
    pub supported_architectures: BTreeSet<String>,
    pub purchasing_capabilities: BTreeSet<PurchasingCapability>,
}

pub(crate) fn provider_pin_json(binding: &ProviderBinding) -> String {
    serde_json::json!({
        "binding_id": binding.id,
        "source_revision": binding.source_revision,
        "binary_sha256": binding.executable_sha256,
        "configuration_sha256": binding.configuration_sha256,
    })
    .to_string()
}

#[derive(Clone, Copy, Debug)]
pub struct ProcessLimits {
    pub stdin_bytes: usize,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

impl Default for ProcessLimits {
    fn default() -> Self {
        Self {
            stdin_bytes: 2 * 1024 * 1024,
            stdout_bytes: 4 * 1024 * 1024,
            stderr_bytes: 256 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProviderRequirements {
    pub os: String,
    pub architecture: String,
    pub purchasing: PurchasingCapability,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LocalFailure {
    #[error("invalid provider registry")]
    InvalidRegistry,
    #[error("provider binding not found")]
    UnknownBinding,
    #[error("provider request is unsupported")]
    UnsupportedRequest,
    #[error("provider capability is unavailable")]
    CapabilityUnavailable,
    #[error("provider pin validation failed")]
    PinValidation,
    #[error("provider version does not match its pin")]
    VersionMismatch,
    #[error("provider process could not be started")]
    Spawn,
    #[error("provider input is too large")]
    InputLimit,
    #[error("provider output is invalid")]
    InvalidResult,
    #[error("provider output is too large")]
    OutputLimit,
    #[error("provider process could not be supervised")]
    Supervision,
    #[error("administrator allocation admission is closed")]
    AdmissionClosed,
    #[error("provider deadline expired")]
    Deadline,
    #[error("provider process supervision requires a Unix host")]
    UnsupportedHost,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("pinned provider process failed")]
pub struct OpaqueChildFailure {
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("provider operation may have completed remotely")]
pub struct AmbiguousOutcome;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ProviderError {
    #[error(transparent)]
    Local(#[from] LocalFailure),
    #[error(transparent)]
    Child(#[from] OpaqueChildFailure),
    #[error(transparent)]
    Ambiguous(#[from] AmbiguousOutcome),
}

#[derive(Clone, Debug)]
pub enum ProviderResult {
    Instance(ProviderInstance),
    Instances(Vec<ProviderInstance>),
    Empty,
    Version(String),
}

#[derive(Clone)]
pub struct ProviderRegistry {
    bindings: HashMap<String, ProviderBinding>,
    environment_snapshot: BTreeMap<OsString, OsString>,
    limits: ProcessLimits,
}

impl ProviderRegistry {
    /// Build a registry from administrator pins and one immutable environment snapshot.
    ///
    /// # Errors
    /// Returns `InvalidRegistry` for invalid pins, duplicate IDs, or an unsafe environment
    /// allowlist. Returns `UnsupportedHost` outside Unix process-group supervision.
    pub fn new(
        bindings: Vec<ProviderBinding>,
        environment_snapshot: BTreeMap<OsString, OsString>,
        limits: ProcessLimits,
    ) -> Result<Self, ProviderError> {
        #[cfg(not(unix))]
        return Err(LocalFailure::UnsupportedHost.into());

        #[cfg(unix)]
        {
            if limits.stdin_bytes == 0 || limits.stdout_bytes == 0 || limits.stderr_bytes == 0 {
                return Err(LocalFailure::InvalidRegistry.into());
            }

            let mut registered = HashMap::new();
            for binding in bindings {
                if !valid_binding_metadata(&binding)
                    || binding.inherited_environment.iter().any(|name| {
                        name.starts_with("GARM")
                            || name.is_empty()
                            || name.bytes().any(|byte| byte == b'=' || byte == 0)
                    })
                    || binding.inherited_environment.iter().any(|name| {
                        !environment_snapshot
                            .keys()
                            .any(|snapshot_name| snapshot_name == OsStr::new(name))
                    })
                    || registered.insert(binding.id.clone(), binding).is_some()
                {
                    return Err(LocalFailure::InvalidRegistry.into());
                }
            }

            Ok(Self {
                bindings: registered,
                environment_snapshot,
                limits,
            })
        }
    }

    /// Validate a selected binding and its version, then invoke the pinned child.
    ///
    /// # Errors
    /// Returns sanitized local, opaque child, or ambiguous failure categories. Any Create
    /// failure after spawn is ambiguous because the remote operation may have completed.
    pub async fn invoke(
        &self,
        binding_id: &str,
        request: &ProviderRequest,
        requirements: Option<&ProviderRequirements>,
        admission: Option<&crate::config::AdmissionSettings>,
        deadline: Instant,
    ) -> Result<ProviderResult, ProviderError> {
        let binding = self
            .bindings
            .get(binding_id)
            .ok_or(LocalFailure::UnknownBinding)?;
        let command = ProviderCommand::parse(&request.command)?;
        validate_request(command, request)?;
        let requirements = if command != ProviderCommand::GetVersion {
            Some(requirements.ok_or(LocalFailure::CapabilityUnavailable)?)
        } else {
            None
        };

        if let Some(requirements) = requirements {
            if !binding.supported_os.contains(&requirements.os)
                || !binding
                    .supported_architectures
                    .contains(&requirements.architecture)
                || !binding
                    .purchasing_capabilities
                    .contains(&requirements.purchasing)
            {
                return Err(LocalFailure::CapabilityUnavailable.into());
            }

            if command == ProviderCommand::CreateInstance {
                let bootstrap = request
                    .bootstrap
                    .as_ref()
                    .ok_or(LocalFailure::UnsupportedRequest)?;
                if bootstrap.name.trim().is_empty()
                    || bootstrap.pool_id != request.pool_id.as_deref().unwrap_or_default()
                    || bootstrap.os_type != requirements.os
                    || bootstrap.os_arch != requirements.architecture
                {
                    return Err(LocalFailure::UnsupportedRequest.into());
                }
            }
        }

        validate_binding_files(binding, deadline).await?;

        let version_request = ProviderRequest {
            command: "GetVersion".into(),
            interface_version: GARM_INTERFACE_VERSION.into(),
            controller_id: request.controller_id.clone(),
            pool_id: None,
            instance_id: None,
            bootstrap: None,
            pool_extra_specs: None,
        };

        let version_output = run_process(
            binding,
            &version_request,
            &self.environment_snapshot,
            self.limits,
            admission,
            deadline,
        )
        .await
        .map_err(map_pre_operation_error)?;
        let version =
            String::from_utf8(version_output.stdout).map_err(|_| LocalFailure::VersionMismatch)?;
        if !version_output.status.success() || version != binding.expected_version {
            return Err(LocalFailure::VersionMismatch.into());
        }

        if command == ProviderCommand::GetVersion {
            return Ok(ProviderResult::Version(version));
        }

        let output = run_process(
            binding,
            request,
            &self.environment_snapshot,
            self.limits,
            admission,
            deadline,
        )
        .await?;
        if !output.status.success() {
            let failure = ProviderError::Child(OpaqueChildFailure {
                exit_code: output.status.code(),
            });
            return Err(map_operation_error(failure, command));
        }

        decode_result(command, output.stdout).map_err(|error| map_operation_error(error, command))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderCommand {
    GetVersion,
    CreateInstance,
    GetInstance,
    ListInstances,
    DeleteInstance,
    StartInstance,
    StopInstance,
    RemoveAllInstances,
}

impl ProviderCommand {
    fn parse(command: &str) -> Result<Self, ProviderError> {
        match command {
            "GetVersion" => Ok(Self::GetVersion),
            "CreateInstance" => Ok(Self::CreateInstance),
            "GetInstance" => Ok(Self::GetInstance),
            "ListInstances" => Ok(Self::ListInstances),
            "DeleteInstance" => Ok(Self::DeleteInstance),
            "StartInstance" => Ok(Self::StartInstance),
            "StopInstance" => Ok(Self::StopInstance),
            "RemoveAllInstances" => Ok(Self::RemoveAllInstances),
            _ => Err(LocalFailure::UnsupportedRequest.into()),
        }
    }

    fn takes_instance_id(self) -> bool {
        matches!(
            self,
            Self::GetInstance | Self::DeleteInstance | Self::StartInstance | Self::StopInstance
        )
    }

    fn takes_pool_id(self) -> bool {
        matches!(self, Self::CreateInstance | Self::ListInstances)
    }

    fn is_create(self) -> bool {
        self == Self::CreateInstance
    }

    fn returns_instance(self) -> bool {
        matches!(self, Self::CreateInstance | Self::GetInstance)
    }
}

async fn validate_binding_files(
    binding: &ProviderBinding,
    deadline: Instant,
) -> Result<(), ProviderError> {
    if !binding.executable_path.is_absolute() || !binding.configuration_path.is_absolute() {
        return Err(LocalFailure::PinValidation.into());
    }

    for (path, expected, executable) in [
        (&binding.executable_path, &binding.executable_sha256, true),
        (
            &binding.configuration_path,
            &binding.configuration_sha256,
            false,
        ),
    ] {
        let actual = timeout_at(deadline, hash_regular_file(path, executable))
            .await
            .map_err(|_| LocalFailure::Deadline)??;
        if actual != *expected {
            return Err(LocalFailure::PinValidation.into());
        }
    }

    Ok(())
}

// Binding files are administrator-managed and must stay immutable while a registry is active.
// Dispatch-time hashes detect accidental drift; they cannot close a hostile replacement race.
async fn hash_regular_file(path: &Path, executable: bool) -> Result<String, ProviderError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|_| LocalFailure::PinValidation)?;
    if !metadata.is_file() {
        return Err(LocalFailure::PinValidation.into());
    }

    #[cfg(unix)]
    if executable && metadata.permissions().mode() & 0o111 == 0 {
        return Err(LocalFailure::PinValidation.into());
    }

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| LocalFailure::PinValidation)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .await
            .map_err(|_| LocalFailure::PinValidation)?;
        if count == 0 {
            break;
        }

        digest.update(&buffer[..count]);
    }

    Ok(hex::encode(digest.finalize()))
}

fn valid_binding_metadata(binding: &ProviderBinding) -> bool {
    !binding.id.is_empty()
        && !binding.cloud.is_empty()
        && !binding.account.is_empty()
        && !binding.location.is_empty()
        && binding.executable_path.is_absolute()
        && binding.configuration_path.is_absolute()
        && is_sha256(&binding.executable_sha256)
        && is_sha256(&binding.configuration_sha256)
        && binding.source_revision.len() == 40
        && binding
            .source_revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && !binding.expected_version.is_empty()
        && binding.expected_version.len() <= 128
        && binding
            .expected_version
            .bytes()
            .all(|byte| byte.is_ascii_graphic())
        && !binding.supported_os.is_empty()
        && !binding.supported_architectures.is_empty()
        && !binding.purchasing_capabilities.is_empty()
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_request(
    command: ProviderCommand,
    request: &ProviderRequest,
) -> Result<(), ProviderError> {
    if request.controller_id.trim().is_empty()
        || (command.takes_pool_id() && request.pool_id.as_deref().is_none_or(str::is_empty))
        || (command.takes_instance_id() && request.instance_id.as_deref().is_none_or(str::is_empty))
        || (command.is_create() && request.bootstrap.is_none())
        || (!command.is_create() && request.bootstrap.is_some())
    {
        return Err(LocalFailure::UnsupportedRequest.into());
    }

    Ok(())
}

fn decode_result(
    command: ProviderCommand,
    stdout: Vec<u8>,
) -> Result<ProviderResult, ProviderError> {
    if command == ProviderCommand::GetVersion {
        let version = String::from_utf8(stdout).map_err(|_| LocalFailure::InvalidResult)?;
        return Ok(ProviderResult::Version(version));
    }

    if command.returns_instance() {
        if stdout.is_empty() {
            return Err(LocalFailure::InvalidResult.into());
        }

        let instance = decode_instance(&stdout)?;
        return Ok(ProviderResult::Instance(instance));
    }

    match command {
        ProviderCommand::ListInstances => {
            let value: serde_json::Value =
                serde_json::from_slice(&stdout).map_err(|_| LocalFailure::InvalidResult)?;
            if value.is_null() {
                return Ok(ProviderResult::Instances(Vec::new()));
            }

            let values = value.as_array().ok_or(LocalFailure::InvalidResult)?;
            let mut instances = Vec::with_capacity(values.len());
            for value in values {
                let bytes = serde_json::to_vec(value).map_err(|_| LocalFailure::InvalidResult)?;
                instances.push(decode_instance(&bytes)?);
            }

            Ok(ProviderResult::Instances(instances))
        }

        _ if stdout.is_empty() => Ok(ProviderResult::Empty),
        _ => Err(LocalFailure::InvalidResult.into()),
    }
}

fn decode_instance(bytes: &[u8]) -> Result<ProviderInstance, ProviderError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| LocalFailure::InvalidResult)?;
    let object = value.as_object().ok_or(LocalFailure::InvalidResult)?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "provider_id"
                | "name"
                | "os_type"
                | "os_name"
                | "os_version"
                | "os_arch"
                | "addresses"
                | "status"
                | "provider_fault"
        )
    }) {
        return Err(LocalFailure::InvalidResult.into());
    }

    let mut instance: ProviderInstance =
        serde_json::from_value(value).map_err(|_| LocalFailure::InvalidResult)?;
    if instance.provider_id.trim().is_empty() || instance.name.trim().is_empty() {
        return Err(LocalFailure::InvalidResult.into());
    }

    instance.provider_fault = None;
    Ok(instance)
}

fn map_pre_operation_error(error: ProviderError) -> ProviderError {
    match error {
        ProviderError::Child(_) => LocalFailure::VersionMismatch.into(),
        ProviderError::Ambiguous(_) => LocalFailure::VersionMismatch.into(),
        ProviderError::Local(LocalFailure::Deadline) => LocalFailure::Deadline.into(),
        ProviderError::Local(error) => error.into(),
    }
}

fn map_operation_error(error: ProviderError, command: ProviderCommand) -> ProviderError {
    if command == ProviderCommand::CreateInstance {
        return AmbiguousOutcome.into();
    }

    error
}
