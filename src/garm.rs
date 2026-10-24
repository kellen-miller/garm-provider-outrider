use std::{
    env,
    io::{self, Read, Write},
    process::ExitCode,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Deserialize, Serialize)]
pub struct BootstrapInstance {
    pub name: String,
    #[serde(default, deserialize_with = "decode_list")]
    pub tools: Vec<RunnerApplicationDownload>,
    #[serde(default)]
    pub repo_url: String,
    #[serde(rename = "callback-url", default)]
    pub callback_url: String,
    #[serde(rename = "metadata-url", default)]
    pub metadata_url: String,
    #[serde(rename = "instance-token", default)]
    pub instance_token: String,
    #[serde(rename = "ssh-keys", default, deserialize_with = "decode_list")]
    pub ssh_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_specs: Option<Value>,
    #[serde(rename = "github-runner-group", default)]
    pub github_runner_group: String,
    #[serde(
        rename = "ca-cert-bundle",
        default,
        deserialize_with = "decode_bytes",
        serialize_with = "encode_bytes"
    )]
    pub ca_cert_bundle: Option<Vec<u8>>,
    #[serde(rename = "arch", default)]
    pub os_arch: String,
    #[serde(default)]
    pub os_type: String,
    #[serde(default)]
    pub flavor: String,
    #[serde(default)]
    pub image: String,
    #[serde(default, deserialize_with = "decode_list")]
    pub labels: Vec<String>,
    #[serde(default)]
    pub pool_id: String,
    #[serde(default)]
    pub user_data_options: UserDataOptions,
    #[serde(default)]
    pub jit_config_enabled: bool,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct RunnerApplicationDownload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_download_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256_checksum: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UserDataOptions {
    #[serde(default)]
    pub disable_updates_on_boot: bool,
    #[serde(default, deserialize_with = "decode_list")]
    pub extra_packages: Vec<String>,
    #[serde(default)]
    pub enable_boot_debug: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderInstance {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub provider_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os_version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os_arch: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<Address>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(
        default,
        deserialize_with = "decode_bytes",
        serialize_with = "encode_bytes",
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_fault: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Address {
    pub address: String,
    #[serde(rename = "type")]
    pub address_type: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ProviderRequest {
    pub command: String,
    pub interface_version: String,
    pub controller_id: String,
    pub pool_id: Option<String>,
    pub instance_id: Option<String>,
    pub bootstrap: Option<BootstrapInstance>,
    pub pool_extra_specs: Option<Value>,
}

/// Immutable pool snapshot identity shared by protocol and service admission.
pub struct PoolSnapshotId(String);

#[derive(Debug, thiserror::Error)]
#[error("invalid or conflicting pool snapshot IDs")]
pub struct PoolSnapshotIdError;

impl PoolSnapshotId {
    /// Resolve only snapshot IDs; unrelated GARM template fields may differ.
    ///
    /// # Errors
    /// Rejects malformed extra specs, non-string/empty IDs, and conflicting IDs.
    pub fn resolve(
        pool_specs: Option<&Value>,
        bootstrap_specs: Option<&Value>,
    ) -> Result<Option<Self>, PoolSnapshotIdError> {
        let mut resolved: Option<&str> = None;
        for specs in [pool_specs, bootstrap_specs].into_iter().flatten() {
            let object = specs.as_object().ok_or(PoolSnapshotIdError)?;
            let Some(value) = object.get("snapshot_id") else {
                continue;
            };
            let id = value
                .as_str()
                .filter(|id| !id.trim().is_empty())
                .ok_or(PoolSnapshotIdError)?;
            if resolved.is_some_and(|previous| previous != id) {
                return Err(PoolSnapshotIdError);
            }

            resolved = Some(id);
        }

        Ok(resolved.map(|id| Self(id.to_owned())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderResponse {
    pub result: Option<Value>,
    pub error: Option<String>,
    pub not_found: bool,
}

pub fn is_provider_invocation() -> bool {
    env::var_os("GARM_COMMAND").is_some()
}

pub async fn run_provider() -> ExitCode {
    match provider_request().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(ProviderFailure::NotFound) => ExitCode::from(30),
        Err(ProviderFailure::Message(message)) => {
            eprintln!("outrider provider: {message}");
            ExitCode::FAILURE
        }
    }
}

enum ProviderFailure {
    NotFound,
    Message(String),
}

struct ProviderConnection {
    address: std::net::SocketAddr,
    token: String,
    timeout: Duration,
}

struct ProviderInput {
    instance_id: Option<String>,
    pool_id: Option<String>,
    pool_extra_specs: Option<Value>,
    bootstrap: Option<BootstrapInstance>,
}

async fn provider_request() -> Result<(), ProviderFailure> {
    let command = env::var("GARM_COMMAND")
        .map_err(|_| ProviderFailure::Message("GARM_COMMAND is required".into()))?;
    let controller_id = env::var("GARM_CONTROLLER_ID")
        .map_err(|_| ProviderFailure::Message("GARM_CONTROLLER_ID is required".into()))?;
    if controller_id.trim().is_empty() {
        return Err(ProviderFailure::Message(
            "GARM_CONTROLLER_ID is required".into(),
        ));
    }

    let config_path = env::var("GARM_PROVIDER_CONFIG_FILE")
        .map_err(|_| ProviderFailure::Message("GARM_PROVIDER_CONFIG_FILE is required".into()))?;
    if !std::path::Path::new(&config_path).is_file() {
        return Err(ProviderFailure::Message(
            "GARM_PROVIDER_CONFIG_FILE does not exist".into(),
        ));
    }

    let interface_version = env::var("GARM_INTERFACE_VERSION")
        .ok()
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "v0.1.0".into());
    if !matches!(interface_version.as_str(), "v0.1.0" | "v0.1.1") {
        return Err(ProviderFailure::Message(
            "unsupported GARM interface version".into(),
        ));
    }

    if write_metadata_response(&command, &interface_version)? {
        return Ok(());
    }

    let connection = provider_connection(&config_path, &command)?;
    let input = provider_input(&command)?;
    let request = ProviderRequest {
        command: command.clone(),
        interface_version,
        controller_id,
        pool_id: input.pool_id,
        instance_id: input.instance_id,
        bootstrap: input.bootstrap,
        pool_extra_specs: input.pool_extra_specs,
    };

    let client = Client::builder()
        .timeout(connection.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ProviderFailure::Message("cannot initialize daemon client".into()))?;
    let response = client
        .post(format!("http://{}/v1/provider", connection.address))
        .bearer_auth(connection.token.trim())
        .json(&request)
        .send()
        .await
        .map_err(|error| daemon_request_failure(error, "daemon request failed"))?;
    let status = response.status();
    let output: ProviderResponse = response
        .json()
        .await
        .map_err(|error| daemon_request_failure(error, "invalid daemon response"))?;
    write_provider_response(&command, status, output)
}

fn daemon_request_failure(error: reqwest::Error, fallback: &'static str) -> ProviderFailure {
    let message = if error.is_timeout() {
        "daemon request timed out"
    } else if error.is_connect() {
        "cannot connect to daemon"
    } else {
        fallback
    };
    ProviderFailure::Message(message.into())
}

fn write_metadata_response(command: &str, version: &str) -> Result<bool, ProviderFailure> {
    let output = match command {
        "GetVersion" => env!("CARGO_PKG_VERSION").as_bytes().to_vec(),
        "GetSupportedInterfaceVersions" if version == "v0.1.1" => {
            br#"["v0.1.0","v0.1.1"]"#.to_vec()
        }

        "GetConfigJSONSchema" if version == "v0.1.1" => {
            serde_json::to_vec(&crate::config::json_schema())
                .map_err(|_| ProviderFailure::Message("cannot encode config schema".into()))?
        }

        "GetExtraSpecsJSONSchema" if version == "v0.1.1" => {
            serde_json::to_vec(&crate::config::extra_specs_json_schema())
                .map_err(|_| ProviderFailure::Message("cannot encode extra specs schema".into()))?
        }

        "GetSupportedInterfaceVersions" | "GetConfigJSONSchema" | "GetExtraSpecsJSONSchema" => {
            return Err(ProviderFailure::Message(
                "command requires GARM interface v0.1.1".into(),
            ));
        }

        _ => return Ok(false),
    };

    io::stdout()
        .write_all(&output)
        .map_err(|_| ProviderFailure::Message("cannot write provider metadata".into()))?;
    Ok(true)
}

fn provider_connection(
    config_path: &str,
    command: &str,
) -> Result<ProviderConnection, ProviderFailure> {
    let configuration = std::fs::read_to_string(config_path)
        .map_err(|_| ProviderFailure::Message("cannot read provider configuration".into()))?;
    let configuration = crate::config::ConfigPatch::from_yaml(&configuration)
        .map_err(|_| ProviderFailure::Message("invalid provider configuration".into()))?;
    let settings = configuration
        .control
        .ok_or_else(|| ProviderFailure::Message("control settings are required".into()))?;
    if settings.provider_timeout_seconds == 0 {
        return Err(ProviderFailure::Message(
            "provider_timeout_seconds must be positive".into(),
        ));
    }

    let address = settings
        .listen_address
        .parse::<std::net::SocketAddr>()
        .map_err(|_| ProviderFailure::Message("invalid daemon address".into()))?;
    if !address.ip().is_loopback() {
        return Err(ProviderFailure::Message(
            "daemon bearer transport must use a loopback address".into(),
        ));
    }

    let token = std::fs::read_to_string(settings.auth_token_file)
        .map_err(|_| ProviderFailure::Message("cannot read internal auth token".into()))?;
    if token.trim().is_empty() {
        return Err(ProviderFailure::Message(
            "internal auth token file is empty".into(),
        ));
    }

    let timeout_seconds = if command == "CreateInstance" {
        let ingress = configuration.ingress.ok_or_else(|| {
            ProviderFailure::Message("ingress settings are required for CreateInstance".into())
        })?;
        let allocation_seconds = ingress.allocation_timeout_seconds;
        let bootstrap_seconds = u64::from(ingress.bootstrap_timeout_minutes)
            .checked_mul(60)
            .ok_or_else(|| {
                ProviderFailure::Message("invalid ingress CreateInstance timeout budget".into())
            })?;
        let margin_seconds = ingress.timeout_margin_seconds;
        if allocation_seconds == 0
            || bootstrap_seconds == 0
            || margin_seconds == 0
            || margin_seconds >= allocation_seconds
            || margin_seconds >= bootstrap_seconds
        {
            return Err(ProviderFailure::Message(
                "invalid ingress CreateInstance timeout budget".into(),
            ));
        }

        allocation_seconds.min(bootstrap_seconds)
    } else {
        settings.provider_timeout_seconds
    };
    let timeout = Duration::from_secs(timeout_seconds);
    if command == "CreateInstance" && Instant::now().checked_add(timeout).is_none() {
        return Err(ProviderFailure::Message(
            "invalid ingress CreateInstance timeout budget".into(),
        ));
    }

    Ok(ProviderConnection {
        address,
        token,
        timeout,
    })
}

fn provider_input(command: &str) -> Result<ProviderInput, ProviderFailure> {
    let mut pool_extra_specs: Option<Value> = env::var("GARM_POOL_EXTRASPECS")
        .ok()
        .map(|value| {
            let bytes = STANDARD
                .decode(value)
                .map_err(|_| ProviderFailure::Message("invalid pool extra specs".into()))?;
            serde_json::from_slice(&bytes)
                .map_err(|_| ProviderFailure::Message("invalid pool extra specs".into()))
        })
        .transpose()?;
    let bootstrap: Option<BootstrapInstance> = if command == "CreateInstance" {
        let mut body = Vec::new();
        io::stdin()
            .read_to_end(&mut body)
            .map_err(|_| ProviderFailure::Message("cannot read bootstrap input".into()))?;
        Some(
            serde_json::from_slice(&body)
                .map_err(|_| ProviderFailure::Message("invalid bootstrap JSON".into()))?,
        )
    } else {
        None
    };
    let bootstrap_specs = bootstrap
        .as_ref()
        .and_then(|bootstrap| bootstrap.extra_specs.as_ref());
    let snapshot_id = PoolSnapshotId::resolve(pool_extra_specs.as_ref(), bootstrap_specs)
        .map_err(|error| ProviderFailure::Message(error.to_string()))?;
    if let Some(snapshot_id) = snapshot_id {
        let specs = pool_extra_specs.get_or_insert_with(|| {
            bootstrap_specs
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}))
        });
        specs
            .as_object_mut()
            .ok_or_else(|| ProviderFailure::Message("invalid pool extra specs".into()))?
            .insert("snapshot_id".to_owned(), Value::String(snapshot_id.0));
    }

    Ok(ProviderInput {
        instance_id: env::var("GARM_INSTANCE_ID").ok(),
        pool_id: env::var("GARM_POOL_ID").ok(),
        pool_extra_specs,
        bootstrap,
    })
}

fn write_provider_response(
    command: &str,
    status: reqwest::StatusCode,
    output: ProviderResponse,
) -> Result<(), ProviderFailure> {
    if status == reqwest::StatusCode::NOT_FOUND && output.not_found && command == "DeleteInstance" {
        return Err(ProviderFailure::NotFound);
    }

    if !status.is_success() {
        if let Some(
            message @ ("allocation concurrency limit reached"
            | "administrator allocation admission is closed"),
        ) = output.error.as_deref()
        {
            return Err(ProviderFailure::Message(format!(
                "{message} (HTTP {})",
                status.as_u16()
            )));
        }

        return Err(ProviderFailure::Message(format!(
            "daemon returned HTTP {}",
            status.as_u16()
        )));
    }

    if let Some(message) = output.error {
        return Err(ProviderFailure::Message(message));
    }

    let result = match command {
        "CreateInstance" | "GetInstance" => Some(("provider instance", output.result.as_ref())),
        "ListInstances" => Some(("instance list", output.result.as_ref())),
        _ => None,
    };

    if let Some((description, value)) = result {
        let value = value
            .ok_or_else(|| ProviderFailure::Message(format!("daemon omitted {description}")))?;
        println!(
            "{}",
            serde_json::to_string(value).map_err(|_| {
                ProviderFailure::Message(format!("cannot encode {description}"))
            })?
        );
    }

    Ok(())
}

// Go nil slices arrive as JSON null; normalize collections at this protocol boundary.
fn decode_list<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

fn decode_bytes<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)?
        .map(|value| STANDARD.decode(value).map_err(serde::de::Error::custom))
        .transpose()
}

fn encode_bytes<S>(value: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    value
        .as_ref()
        .map(|bytes| STANDARD.encode(bytes))
        .serialize(serializer)
}

#[cfg(test)]
mod tests {
    use super::BootstrapInstance;
    use serde_json::{Value, json};

    #[test]
    fn go_nullable_collections_normalize_without_accepting_wrong_types() {
        let bootstrap = json!({
            "name": "runner", "tools": [], "ssh-keys": [], "labels": [],
            "user_data_options": {"extra_packages": []}
        });
        for (path, populated) in [
            ("/tools", json!([{"os":"linux"}])),
            ("/ssh-keys", json!(["ssh-ed25519 fixture"])),
            ("/labels", json!(["routing-label"])),
            ("/user_data_options/extra_packages", json!(["git"])),
        ] {
            for value in [Value::Null, json!([]), populated] {
                let mut input = bootstrap.clone();
                *input.pointer_mut(path).expect("collection field") = value.clone();
                let decoded: BootstrapInstance = serde_json::from_value(input).expect("Go slice");
                let normalized = serde_json::to_value(decoded).expect("normalized bootstrap");
                assert_eq!(
                    normalized.pointer(path),
                    Some(&if value.is_null() { json!([]) } else { value })
                );
            }

            for invalid in [json!(false), json!({}), json!("value"), json!([false])] {
                let mut input = bootstrap.clone();
                *input.pointer_mut(path).expect("collection field") = invalid;
                assert!(serde_json::from_value::<BootstrapInstance>(input).is_err());
            }
        }
    }
}
