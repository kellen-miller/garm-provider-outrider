use std::collections::BTreeMap;
use std::fmt;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::Value;

use crate::garm::{BootstrapInstance, UserDataOptions};

const MACOS_TEMPLATE: &str = include_str!("../templates/macos.sh");
const LINUX_HOOK_TEMPLATE: &str = include_str!("../templates/linux.sh");
const WINDOWS_HOOK_TEMPLATE: &str = include_str!("../templates/windows.ps1");

const AWS_USER_DATA_LIMIT_BYTES: usize = 16 * 1024;
const AZURE_CUSTOM_DATA_LIMIT_BYTES: usize = 65_535;
const GCP_METADATA_VALUE_LIMIT_BYTES: usize = 262_144;
const GCP_METADATA_TOTAL_LIMIT_BYTES: usize = 524_288;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootstrapPlatform {
    LinuxAmd64,
    LinuxArm64,
    WindowsAmd64,
    MacosArm64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootstrapProvider {
    Aws,
    Azure,
    Gcp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootstrapTarget {
    pub platform: BootstrapPlatform,
    pub macos_user_data_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapError(&'static str);

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for BootstrapError {}

pub struct ProviderBootstrapHook {
    pub platform: BootstrapPlatform,
    pub requires_cloud_init: bool,
    pub user_data_options: UserDataOptions,
    provider: BootstrapProvider,
    runner_install_template: SecretString,
    extra_context: BTreeMap<String, String>,
    pub wrapped_payload_bound_bytes: usize,
    pub largest_metadata_value_bound_bytes: usize,
    pub transport_limit_bytes: usize,
    pub metadata_value_limit_bytes: usize,
}

impl fmt::Debug for ProviderBootstrapHook {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderBootstrapHook")
            .field("platform", &self.platform)
            .field("provider", &self.provider)
            .field("runner_install_template", &"[REDACTED]")
            .field("extra_context", &"[REDACTED]")
            .field(
                "wrapped_payload_bound_bytes",
                &self.wrapped_payload_bound_bytes,
            )
            .field(
                "largest_metadata_value_bound_bytes",
                &self.largest_metadata_value_bound_bytes,
            )
            .field("transport_limit_bytes", &self.transport_limit_bytes)
            .field(
                "metadata_value_limit_bytes",
                &self.metadata_value_limit_bytes,
            )
            .finish()
    }
}

impl ProviderBootstrapHook {
    /// Return the provider's approved byte-template and checksum extra specs.
    pub fn extra_specs_json(&self) -> Value {
        let mut specs = serde_json::json!({
            "runner_install_template": STANDARD.encode(self.runner_install_template.expose_secret().as_bytes()),
            "extra_context": self.extra_context,
            "disable_updates": self.user_data_options.disable_updates_on_boot,
            "enable_boot_debug": self.user_data_options.enable_boot_debug,
        });
        if matches!(
            self.provider,
            BootstrapProvider::Aws | BootstrapProvider::Azure
        ) {
            specs["extra_packages"] = serde_json::json!(self.user_data_options.extra_packages);
        }
        specs
    }
}

#[derive(Deserialize)]
struct ExtraSpecs {
    #[serde(default)]
    snapshot_id: Option<String>,
    #[serde(default)]
    runner_install_template: Option<Value>,
    #[serde(default)]
    pre_install_scripts: BTreeMap<String, String>,
    #[serde(default)]
    extra_context: BTreeMap<String, String>,
}

struct SelectedArtifact<'a> {
    download_url: &'a str,
    download_token: &'a str,
    filename: &'a str,
    checksum: String,
}

/// Render the standalone macOS bootstrap script.
///
/// # Errors
///
/// Returns an error for unsupported platforms or options, invalid URLs, or
/// artifacts.
pub fn generate_macos(
    instance: &BootstrapInstance,
    target: BootstrapTarget,
) -> Result<SecretString, BootstrapError> {
    let platform = platform_for(instance)?;
    if platform != BootstrapPlatform::MacosArm64 {
        return Err(BootstrapError(
            "standalone bootstrap is supported only for macOS",
        ));
    }
    validate_bootstrap_request(instance, target, platform)?;
    let artifact = select_artifact(instance, platform)?;
    Ok(SecretString::from(render_install_hook_script(
        instance, platform, &artifact,
    )?))
}

/// Generate a trusted install hook for a pinned GARM provider executable.
///
/// The returned Go template evaluates to the rendered script as one literal.
/// Its byte bound includes the common wrapper and bootstrap-derived delivery
/// fields. It assumes the caller sends only catalog-approved provider extras;
/// current approved AWS, Azure, and GCP extras do not add user-data metadata.
///
/// # Errors
///
/// Returns an error for unsupported platforms, invalid bootstrap fields, or
/// provider payloads whose conservative bound exceeds the transport limit.
pub fn generate_provider_hook(
    instance: &BootstrapInstance,
    target: BootstrapTarget,
    provider: BootstrapProvider,
) -> Result<ProviderBootstrapHook, BootstrapError> {
    let platform = platform_for(instance)?;
    if platform == BootstrapPlatform::MacosArm64 {
        return Err(BootstrapError(
            "macOS provider bootstrap is unsupported by pinned GARM providers",
        ));
    }

    validate_bootstrap_request(instance, target, platform)?;
    let artifact = select_artifact(instance, platform)?;
    let script = render_install_hook_script(instance, platform, &artifact)?;
    let mut user_data_options = instance.user_data_options.clone();
    if matches!(
        platform,
        BootstrapPlatform::LinuxAmd64 | BootstrapPlatform::LinuxArm64
    ) {
        user_data_options.extra_packages = normalized_linux_packages(&user_data_options);
    }
    let template = format!(
        "{{{{{}}}}}",
        serde_json::to_string(&script)
            .map_err(|_| BootstrapError("cannot quote provider install template"))?
    );
    let script_len = script.len();
    let common_linux_bound = || {
        let script_base64_len = STANDARD.encode(script.as_bytes()).len();
        let ca_len = instance.ca_cert_bundle.as_deref().unwrap_or_default().len();
        let ssh_len = instance.ssh_keys.iter().map(String::len).sum::<usize>();
        let packages_len = user_data_options
            .extra_packages
            .iter()
            .map(String::len)
            .sum::<usize>();
        script_base64_len
            .saturating_add(2048)
            .saturating_add(ca_len.saturating_mul(6))
            .saturating_add(ssh_len.saturating_mul(6))
            .saturating_add(packages_len.saturating_mul(6))
            .saturating_add(instance.ssh_keys.len().saturating_mul(16))
            .saturating_add(user_data_options.extra_packages.len().saturating_mul(16))
    };
    let (
        wrapped_payload_bound_bytes,
        largest_metadata_value_bound_bytes,
        transport_limit_bytes,
        metadata_value_limit_bytes,
    ) = match (provider, platform) {
        (BootstrapProvider::Aws, BootstrapPlatform::LinuxAmd64 | BootstrapPlatform::LinuxArm64) => {
            (
                common_linux_bound(),
                common_linux_bound(),
                AWS_USER_DATA_LIMIT_BYTES,
                AWS_USER_DATA_LIMIT_BYTES,
            )
        }
        (BootstrapProvider::Aws, BootstrapPlatform::WindowsAmd64) => {
            // Conservative uncompressed bound; admitted payloads never use child compression.
            let bound = script_len
                .saturating_add("<powershell></powershell>".len())
                .saturating_add(128);
            (
                bound,
                bound,
                AWS_USER_DATA_LIMIT_BYTES,
                AWS_USER_DATA_LIMIT_BYTES,
            )
        }
        (
            BootstrapProvider::Azure,
            BootstrapPlatform::LinuxAmd64 | BootstrapPlatform::LinuxArm64,
        ) => {
            let bound = common_linux_bound();
            (
                bound,
                bound,
                AZURE_CUSTOM_DATA_LIMIT_BYTES,
                AZURE_CUSTOM_DATA_LIMIT_BYTES,
            )
        }
        (BootstrapProvider::Azure, BootstrapPlatform::WindowsAmd64) => {
            let bound = script_len.saturating_add(1024);
            (
                bound,
                bound,
                AZURE_CUSTOM_DATA_LIMIT_BYTES,
                AZURE_CUSTOM_DATA_LIMIT_BYTES,
            )
        }
        (BootstrapProvider::Gcp, BootstrapPlatform::LinuxAmd64 | BootstrapPlatform::LinuxArm64) => {
            let user_data_bound = common_linux_bound();
            let runner_name_bound = instance.name.len().saturating_add(64);
            let ssh_metadata_bound = instance
                .ssh_keys
                .iter()
                .map(String::len)
                .sum::<usize>()
                .saturating_add(instance.ssh_keys.len().saturating_mul(16))
                .saturating_add(64);
            let total = user_data_bound
                .saturating_add(runner_name_bound)
                .saturating_add(ssh_metadata_bound);
            (
                total,
                user_data_bound
                    .max(runner_name_bound)
                    .max(ssh_metadata_bound),
                GCP_METADATA_TOTAL_LIMIT_BYTES,
                GCP_METADATA_VALUE_LIMIT_BYTES,
            )
        }
        (BootstrapProvider::Gcp, BootstrapPlatform::WindowsAmd64) => {
            let runner_name_bound = instance.name.len().saturating_add(64);
            let ssh_metadata_bound = instance
                .ssh_keys
                .iter()
                .map(String::len)
                .sum::<usize>()
                .saturating_add(instance.ssh_keys.len().saturating_mul(16))
                .saturating_add(64);
            let total = script_len
                .saturating_add(4096)
                .saturating_add(runner_name_bound)
                .saturating_add(ssh_metadata_bound);
            (
                total,
                script_len.max(runner_name_bound).max(ssh_metadata_bound),
                GCP_METADATA_TOTAL_LIMIT_BYTES,
                GCP_METADATA_VALUE_LIMIT_BYTES,
            )
        }
        (_, BootstrapPlatform::MacosArm64) => {
            return Err(BootstrapError("macOS provider bootstrap is unsupported"));
        }
    };
    if wrapped_payload_bound_bytes > transport_limit_bytes
        || largest_metadata_value_bound_bytes > metadata_value_limit_bytes
    {
        return Err(BootstrapError(
            "wrapped runner bootstrap exceeds provider transport limit",
        ));
    }

    let mut extra_context = BTreeMap::new();
    extra_context.insert("outrider_sha256".to_owned(), artifact.checksum);
    Ok(ProviderBootstrapHook {
        platform,
        provider,
        requires_cloud_init: matches!(
            platform,
            BootstrapPlatform::LinuxAmd64 | BootstrapPlatform::LinuxArm64
        ),
        user_data_options,
        runner_install_template: SecretString::from(template),
        extra_context,
        wrapped_payload_bound_bytes,
        largest_metadata_value_bound_bytes,
        transport_limit_bytes,
        metadata_value_limit_bytes,
    })
}

fn validate_bootstrap_request(
    instance: &BootstrapInstance,
    target: BootstrapTarget,
    platform: BootstrapPlatform,
) -> Result<(), BootstrapError> {
    if platform != target.platform {
        return Err(BootstrapError(
            "bootstrap target does not match GARM OS and architecture",
        ));
    }

    if platform == BootstrapPlatform::MacosArm64 && !target.macos_user_data_enabled {
        return Err(BootstrapError(
            "macOS image lacks qualified EC2 Init UserData support",
        ));
    }

    if !instance.github_runner_group.is_empty() {
        return Err(BootstrapError(
            "GARM runner groups are unsupported; use repository registration",
        ));
    }

    if instance.name.is_empty() || instance.instance_token.is_empty() {
        return Err(BootstrapError(
            "GARM runner name and callback token are required",
        ));
    }

    if !valid_http_url(&instance.callback_url) || !valid_http_url(&instance.metadata_url) {
        return Err(BootstrapError(
            "GARM callback and metadata URLs must use HTTP or HTTPS",
        ));
    }

    if !instance.jit_config_enabled && !valid_https_url(&instance.repo_url) {
        return Err(BootstrapError(
            "repository registration requires an HTTPS repository URL",
        ));
    }

    let extra_specs = parse_extra_specs(instance.extra_specs.as_ref())?;
    if extra_specs
        .snapshot_id
        .as_deref()
        .is_some_and(str::is_empty)
    {
        return Err(BootstrapError(
            "GARM snapshot_id must be non-empty when present",
        ));
    }

    if extra_specs.runner_install_template.is_some() || !extra_specs.extra_context.is_empty() {
        return Err(BootstrapError(
            "custom install templates and extra context are unsupported",
        ));
    }

    if !extra_specs.pre_install_scripts.is_empty() {
        return Err(BootstrapError("GARM pre-install scripts are unsupported"));
    }

    if platform != BootstrapPlatform::LinuxAmd64
        && platform != BootstrapPlatform::LinuxArm64
        && !instance.user_data_options.extra_packages.is_empty()
    {
        return Err(BootstrapError("extra packages require Linux cloud-init"));
    }

    for package in &instance.user_data_options.extra_packages {
        if package.trim().is_empty() || package.contains('\n') || package.contains('\0') {
            return Err(BootstrapError(
                "extra package names must be non-empty single-line values",
            ));
        }
    }

    if instance
        .ssh_keys
        .iter()
        .any(|key| key.is_empty() || key.contains(['\n', '\r', '\0']))
    {
        return Err(BootstrapError(
            "SSH keys must be non-empty single-line values",
        ));
    }

    if !instance.jit_config_enabled && instance.labels.is_empty() {
        return Err(BootstrapError(
            "legacy repository registration requires exact GARM labels",
        ));
    }

    Ok(())
}

fn render_install_hook_script(
    instance: &BootstrapInstance,
    platform: BootstrapPlatform,
    artifact: &SelectedArtifact<'_>,
) -> Result<String, BootstrapError> {
    let template = match platform {
        BootstrapPlatform::LinuxAmd64 | BootstrapPlatform::LinuxArm64 => LINUX_HOOK_TEMPLATE,

        BootstrapPlatform::WindowsAmd64 => WINDOWS_HOOK_TEMPLATE,
        BootstrapPlatform::MacosArm64 => MACOS_TEMPLATE,
    };

    let shell_labels = instance.labels.join(",");
    let script_literal = |value: &str| match platform {
        BootstrapPlatform::WindowsAmd64 => powershell_literal(value),
        BootstrapPlatform::LinuxAmd64
        | BootstrapPlatform::LinuxArm64
        | BootstrapPlatform::MacosArm64 => shell_literal(value),
    };

    let ca_bundle = instance.ca_cert_bundle.as_deref().unwrap_or_default();
    let ssh_setup = match platform {
        BootstrapPlatform::WindowsAmd64 => powershell_ssh_setup(&instance.ssh_keys),
        BootstrapPlatform::MacosArm64 => shell_ssh_setup(&instance.ssh_keys),
        BootstrapPlatform::LinuxAmd64 | BootstrapPlatform::LinuxArm64 => String::new(),
    };

    let ca_setup = match platform {
        BootstrapPlatform::WindowsAmd64 => powershell_ca_setup(ca_bundle)?,
        BootstrapPlatform::LinuxAmd64
        | BootstrapPlatform::LinuxArm64
        | BootstrapPlatform::MacosArm64 => String::new(),
    };

    let updates_setup = if platform == BootstrapPlatform::WindowsAmd64
        && instance.user_data_options.disable_updates_on_boot
    {
        "New-Item -ItemType Directory -Force -Path 'HKLM:\\SOFTWARE\\Policies\\Microsoft\\Windows\\WindowsUpdate\\AU' | Out-Null\nSet-ItemProperty -Path 'HKLM:\\SOFTWARE\\Policies\\Microsoft\\Windows\\WindowsUpdate\\AU' -Name NoAutoUpdate -Type DWord -Value 1 -Force\n".to_owned()
    } else {
        String::new()
    };

    let values = vec![
        ("@@CALLBACK_URL@@", script_literal(&instance.callback_url)),
        ("@@METADATA_URL@@", script_literal(&instance.metadata_url)),
        (
            "@@INSTANCE_TOKEN@@",
            script_literal(&instance.instance_token),
        ),
        ("@@RUNNER_NAME@@", script_literal(&instance.name)),
        ("@@DOWNLOAD_URL@@", script_literal(artifact.download_url)),
        (
            "@@DOWNLOAD_TOKEN@@",
            script_literal(artifact.download_token),
        ),
        ("@@FILENAME@@", script_literal(artifact.filename)),
        ("@@SHA256@@", script_literal(&artifact.checksum)),
        ("@@REPOSITORY_URL@@", script_literal(&instance.repo_url)),
        ("@@RUNNER_LABELS@@", script_literal(&shell_labels)),
        (
            "@@JIT_ENABLED@@",
            match platform {
                BootstrapPlatform::WindowsAmd64 => if instance.jit_config_enabled {
                    "$true"
                } else {
                    "$false"
                }
                .to_owned(),
                BootstrapPlatform::LinuxAmd64
                | BootstrapPlatform::LinuxArm64
                | BootstrapPlatform::MacosArm64 => instance.jit_config_enabled.to_string(),
            },
        ),
        ("@@SSH_KEYS@@", ssh_setup),
        ("@@CA_BUNDLE@@", ca_setup),
        (
            "@@CA_BUNDLE_B64@@",
            shell_literal(&STANDARD.encode(ca_bundle)),
        ),
        ("@@DISABLE_UPDATES@@", updates_setup),
    ];
    Ok(render_template(template, &values))
}

fn platform_for(instance: &BootstrapInstance) -> Result<BootstrapPlatform, BootstrapError> {
    match (instance.os_type.as_str(), instance.os_arch.as_str()) {
        ("linux", "amd64") => Ok(BootstrapPlatform::LinuxAmd64),
        ("linux", "arm64") => Ok(BootstrapPlatform::LinuxArm64),
        ("windows", "amd64") => Ok(BootstrapPlatform::WindowsAmd64),
        ("macos", "arm64") => Ok(BootstrapPlatform::MacosArm64),
        _ => Err(BootstrapError(
            "unsupported GARM runner OS and architecture",
        )),
    }
}

fn select_artifact(
    instance: &BootstrapInstance,
    platform: BootstrapPlatform,
) -> Result<SelectedArtifact<'_>, BootstrapError> {
    let (os, architecture) = match platform {
        BootstrapPlatform::LinuxAmd64 => ("linux", "x64"),
        BootstrapPlatform::LinuxArm64 => ("linux", "arm64"),
        BootstrapPlatform::WindowsAmd64 => ("win", "x64"),
        BootstrapPlatform::MacosArm64 => ("osx", "arm64"),
    };

    let mut matches = instance.tools.iter().filter(|tool| {
        tool.os.as_deref() == Some(os) && tool.architecture.as_deref() == Some(architecture)
    });
    let artifact = matches.next().ok_or(BootstrapError(
        "GARM did not provide a matching runner artifact",
    ))?;
    if matches.next().is_some() {
        return Err(BootstrapError(
            "GARM provided duplicate matching runner artifacts",
        ));
    }

    let download_url = artifact
        .download_url
        .as_deref()
        .ok_or(BootstrapError("runner artifact URL is missing"))?;
    let filename = artifact
        .filename
        .as_deref()
        .ok_or(BootstrapError("runner artifact filename is missing"))?;
    if !valid_https_url(download_url) || !valid_artifact_filename(filename) {
        return Err(BootstrapError("runner artifact URL or filename is invalid"));
    }

    let checksum = artifact
        .sha256_checksum
        .as_deref()
        .ok_or(BootstrapError("runner artifact checksum is missing"))?;
    if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(BootstrapError(
            "runner artifact must have a full SHA-256 checksum",
        ));
    }

    Ok(SelectedArtifact {
        download_url,
        download_token: artifact.temp_download_token.as_deref().unwrap_or_default(),
        filename,
        checksum: checksum.to_ascii_lowercase(),
    })
}

fn valid_artifact_filename(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains(['\n', '\r', '\0'])
}

fn parse_extra_specs(value: Option<&Value>) -> Result<ExtraSpecs, BootstrapError> {
    let Some(value) = value else {
        return Ok(ExtraSpecs {
            snapshot_id: None,
            runner_install_template: None,
            pre_install_scripts: BTreeMap::new(),
            extra_context: BTreeMap::new(),
        });
    };

    let object = value
        .as_object()
        .ok_or(BootstrapError("GARM extra_specs must be a JSON object"))?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "snapshot_id" | "runner_install_template" | "pre_install_scripts" | "extra_context"
        )
    }) {
        return Err(BootstrapError(
            "GARM extra_specs contains an unsupported option",
        ));
    }

    serde_json::from_value(value.clone())
        .map_err(|_| BootstrapError("GARM extra_specs has an invalid shape"))
}

fn normalized_linux_packages(options: &UserDataOptions) -> Vec<String> {
    let mut packages = Vec::new();
    for package in ["curl", "tar", "jq", "sudo", "coreutils"]
        .into_iter()
        .chain(options.extra_packages.iter().map(String::as_str))
    {
        if !packages.iter().any(|present| present == package) {
            packages.push(package.to_owned());
        }
    }
    packages
}

fn render_template(template: &str, values: &[(&str, String)]) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut remaining = template;
    while let Some((position, placeholder, value)) = values
        .iter()
        .filter_map(|(placeholder, value)| {
            remaining
                .find(placeholder)
                .map(|position| (position, *placeholder, value.as_str()))
        })
        .min_by_key(|(position, _, _)| *position)
    {
        rendered.push_str(&remaining[..position]);
        rendered.push_str(value);
        remaining = &remaining[position + placeholder.len()..];
    }

    rendered.push_str(remaining);
    rendered
}

fn valid_http_url(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some_and(|host| !host.is_empty())
    })
}

fn valid_https_url(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| url.scheme() == "https" && url.host_str().is_some())
}

fn shell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn powershell_ssh_setup(keys: &[String]) -> String {
    if keys.is_empty() {
        return String::new();
    }

    let values = keys
        .iter()
        .map(|key| format!("    {}", powershell_literal(key)))
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        "\n$sshKeys = @(\n{values}\n)\n$sshPath = 'C:\\ProgramData\\ssh\\administrators_authorized_keys'\nNew-Item -ItemType Directory -Force -Path (Split-Path $sshPath) | Out-Null\n$sshKeys | Add-Content -Path $sshPath\n& icacls.exe $sshPath /inheritance:r /grant 'Administrators:F' /grant 'SYSTEM:F' | Out-Null\nif ($LASTEXITCODE -ne 0) {{ throw 'SSH authorized-key ACL setup failed' }}\n"
    )
}

fn shell_ssh_setup(keys: &[String]) -> String {
    let mut script = String::new();
    if keys.is_empty() {
        return script;
    }

    script.push_str("install -d -o runner -g staff -m 0700 /Users/runner/.ssh\n");
    for key in keys {
        script.push_str(&format!(
            "printf '%s\\n' {} >> /Users/runner/.ssh/authorized_keys\n",
            shell_literal(key)
        ));
    }

    script.push_str(
        "chown runner:staff /Users/runner/.ssh/authorized_keys && chmod 0600 /Users/runner/.ssh/authorized_keys\n",
    );
    script
}

fn powershell_ca_setup(bundle: &[u8]) -> Result<String, BootstrapError> {
    if bundle.is_empty() {
        return Ok(String::new());
    }

    let pem =
        std::str::from_utf8(bundle).map_err(|_| BootstrapError("CA bundle must be PEM text"))?;
    let matches = pem.matches("-----BEGIN CERTIFICATE-----").count();
    if matches == 0 || matches != pem.matches("-----END CERTIFICATE-----").count() {
        return Err(BootstrapError(
            "CA bundle must contain complete PEM certificates",
        ));
    }

    Ok(format!(
        "$caPem = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}'))\n[regex]::Matches($caPem, '(?s)-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----') | ForEach-Object {{\n    $path = Join-Path $env:TEMP ([guid]::NewGuid().ToString() + '.cer')\n    [IO.File]::WriteAllText($path, $_.Value, [Text.Encoding]::ASCII)\n    Import-Certificate -FilePath $path -CertStoreLocation 'Cert:\\LocalMachine\\Root' | Out-Null\n    Remove-Item $path -Force\n}}\n",
        STANDARD.encode(bundle)
    ))
}
