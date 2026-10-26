use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AwsChildConfiguration {
    pub(crate) credentials: AwsChildCredentials,
    pub(crate) subnet_id: String,
    pub(crate) region: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AwsChildCredentials {
    pub(crate) credential_type: String,
    #[serde(rename = "static")]
    pub(crate) static_credentials: Option<AwsStaticCredentials>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AwsStaticCredentials {
    pub(crate) access_key_id: String,
    pub(crate) secret_access_key: String,
    pub(crate) session_token: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct AzureChildConfiguration {
    pub(crate) credentials: AzureChildCredentials,
    pub(crate) location: String,
    #[serde(default)]
    pub(crate) disable_isolated_networks: bool,
    #[serde(default)]
    pub(crate) vnet_subnet_id: String,
    #[serde(default)]
    pub(crate) use_ephemeral_storage: bool,
}

#[derive(Deserialize)]
pub(crate) struct AzureChildCredentials {
    pub(crate) subscription_id: String,
    #[serde(default)]
    pub(crate) client_options: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub(crate) service_principal: Option<AzureServicePrincipal>,
    #[serde(default)]
    pub(crate) workload_identity: Option<AzureUnsupportedIdentity>,
    #[serde(default)]
    pub(crate) managed_identity: Option<AzureUnsupportedIdentity>,
}

#[derive(Deserialize)]
pub(crate) struct AzureServicePrincipal {
    pub(crate) tenant_id: String,
    pub(crate) client_id: String,
    pub(crate) client_secret: String,
}

#[derive(Deserialize)]
pub(crate) struct AzureUnsupportedIdentity {}

#[derive(Deserialize)]
pub(crate) struct GcpChildConfiguration {
    pub(crate) project_id: String,
    pub(crate) zone: String,
    pub(crate) credentials_file: String,
    pub(crate) network_id: String,
    pub(crate) subnetwork_id: String,
    #[serde(default)]
    pub(crate) external_ip_access: bool,
    #[serde(default)]
    pub(crate) enable_regional_placement: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NativeCloudScope {
    Aws {
        region: String,
        subnet_id: String,
    },
    Azure {
        subscription_id: String,
        location: String,
        vnet_subnet_id: Option<String>,
    },
    Gcp {
        project_id: String,
        zone: String,
        region: String,
    },
}

/// Read a bounded, pinned native file before interpreting any credentials.
pub(crate) fn read_pinned_configuration(
    path: &std::path::Path,
    expected_sha256: &str,
) -> Result<Vec<u8>, &'static str> {
    use sha2::{Digest, Sha256};
    let bytes = read_configuration_file(path)?;
    if !hex::encode(Sha256::digest(&bytes)).eq_ignore_ascii_case(expected_sha256) {
        return Err("native provider configuration does not match its fingerprint");
    }

    Ok(bytes)
}

fn read_configuration_file(path: &std::path::Path) -> Result<Vec<u8>, &'static str> {
    use std::io::Read;
    const MAX_CONFIGURATION_BYTES: u64 = 1024 * 1024;
    if !path.is_absolute() {
        return Err("native provider configuration path must be absolute");
    }

    let file = std::fs::File::open(path).map_err(|_| "native configuration could not be read")?;
    let metadata = file
        .metadata()
        .map_err(|_| "native configuration could not be read")?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIGURATION_BYTES {
        return Err("native configuration exceeds its file bounds");
    }

    let mut bytes = Vec::new();
    file.take(MAX_CONFIGURATION_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "native configuration could not be read")?;
    if bytes.len() as u64 > MAX_CONFIGURATION_BYTES {
        return Err("native configuration exceeds its file bounds");
    }

    Ok(bytes)
}

pub(crate) fn load_native_cloud_scope(
    cloud: &str,
    path: &std::path::Path,
    expected_sha256: &str,
) -> Result<NativeCloudScope, &'static str> {
    let bytes = read_pinned_configuration(path, expected_sha256)?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| "native provider configuration is invalid")?;
    match cloud {
        "aws" => {
            let child: AwsChildConfiguration =
                toml_edit::de::from_str(text).map_err(|_| "native AWS configuration is invalid")?;
            if child.region.trim().is_empty()
                || child.subnet_id.trim().is_empty()
                || child.credentials.credential_type != "static"
                || child
                    .credentials
                    .static_credentials
                    .as_ref()
                    .is_none_or(|credentials| {
                        credentials.access_key_id.trim().is_empty()
                            || credentials.secret_access_key.trim().is_empty()
                    })
            {
                return Err(
                    "native AWS configuration requires a region, subnet and static credentials",
                );
            }

            Ok(NativeCloudScope::Aws {
                region: child.region,
                subnet_id: child.subnet_id,
            })
        }
        "azure" => {
            let child: AzureChildConfiguration = toml_edit::de::from_str(text)
                .map_err(|_| "native Azure configuration is invalid")?;
            if child.location.trim().is_empty()
                || child.credentials.subscription_id.trim().is_empty()
                || child.use_ephemeral_storage
                || (child.disable_isolated_networks && child.vnet_subnet_id.trim().is_empty())
                || !child.credentials.client_options.is_empty()
                || child.credentials.workload_identity.is_some()
                || child.credentials.managed_identity.is_some()
                || child
                    .credentials
                    .service_principal
                    .as_ref()
                    .is_none_or(|principal| {
                        principal.tenant_id.trim().is_empty()
                            || principal.client_id.trim().is_empty()
                            || principal.client_secret.trim().is_empty()
                    })
            {
                return Err(
                    "native Azure configuration requires supported networking and service-principal credentials",
                );
            }

            Ok(NativeCloudScope::Azure {
                subscription_id: child.credentials.subscription_id,
                location: child.location,
                vnet_subnet_id: child
                    .disable_isolated_networks
                    .then_some(child.vnet_subnet_id),
            })
        }
        "gcp" => {
            let child: GcpChildConfiguration =
                toml_edit::de::from_str(text).map_err(|_| "native GCP configuration is invalid")?;
            let region = child
                .zone
                .rsplit_once('-')
                .filter(|(region, zone)| {
                    !region.is_empty()
                        && zone.len() == 1
                        && zone.bytes().all(|byte| byte.is_ascii_lowercase())
                })
                .map(|(region, _)| region.to_owned())
                .ok_or("native GCP zone is invalid")?;
            if child.project_id.trim().is_empty()
                || child.network_id.trim().is_empty()
                || child.subnetwork_id.trim().is_empty()
                || child.enable_regional_placement
            {
                return Err(
                    "native GCP configuration requires explicit project, zone and network scope",
                );
            }

            let key = read_configuration_file(std::path::Path::new(&child.credentials_file))?;
            let key: serde_json::Value = serde_json::from_slice(&key)
                .map_err(|_| "native GCP service-account key is invalid")?;
            if key.get("type").and_then(serde_json::Value::as_str) != Some("service_account") {
                return Err("native GCP configuration requires an explicit service-account key");
            }

            Ok(NativeCloudScope::Gcp {
                project_id: child.project_id,
                zone: child.zone,
                region,
            })
        }
        _ => Err("native provider cloud is unsupported"),
    }
}

#[cfg(test)]
mod tests {
    use super::{NativeCloudScope, load_native_cloud_scope};
    use sha2::{Digest, Sha256};
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn native_scopes_accept_only_explicit_supported_auth_and_features() {
        let directory = std::env::temp_dir().join(format!(
            "outrider-native-scope-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let key = directory.join("service-account.json");
        fs::write(
            &key,
            r#"{"type":"service_account","private_key":"secret-key-canary"}"#,
        )
        .unwrap();
        let aws = "region = 'us-east-1'\nsubnet_id = 'subnet-native'\n[credentials]\ncredential_type = 'static'\n[credentials.static]\naccess_key_id = 'explicit-key'\nsecret_access_key = 'secret-canary'\n";
        let azure = "location = 'eastus'\ndisable_isolated_networks = true\nvnet_subnet_id = '/native/subnet'\n[credentials]\nsubscription_id = 'subscription-native'\n[credentials.service_principal]\ntenant_id = 'tenant'\nclient_id = 'client'\nclient_secret = 'secret-canary'\n";
        let gcp = format!(
            "project_id = 'native-project'\nzone = 'us-central1-a'\ncredentials_file = {:?}\nnetwork_id = 'network'\nsubnetwork_id = 'subnet'\n",
            key.to_string_lossy()
        );
        for (cloud, text, expected) in [
            (
                "aws",
                aws,
                NativeCloudScope::Aws {
                    region: "us-east-1".into(),
                    subnet_id: "subnet-native".into(),
                },
            ),
            (
                "azure",
                azure,
                NativeCloudScope::Azure {
                    subscription_id: "subscription-native".into(),
                    location: "eastus".into(),
                    vnet_subnet_id: Some("/native/subnet".into()),
                },
            ),
            (
                "gcp",
                gcp.as_str(),
                NativeCloudScope::Gcp {
                    project_id: "native-project".into(),
                    zone: "us-central1-a".into(),
                    region: "us-central1".into(),
                },
            ),
        ] {
            let path = directory.join(format!("{cloud}.toml"));
            fs::write(&path, text).unwrap();
            assert_eq!(
                load_native_cloud_scope(cloud, &path, &hex::encode(Sha256::digest(text))),
                Ok(expected)
            );
        }

        for (cloud, text) in [
            (
                "aws",
                aws.replace("credential_type = 'static'", "credential_type = 'role'"),
            ),
            (
                "azure",
                format!("{azure}\n[credentials.managed_identity]\n"),
            ),
            ("azure", format!("use_ephemeral_storage = true\n{azure}")),
            ("gcp", format!("enable_regional_placement = true\n{gcp}")),
            (
                "gcp",
                gcp.replace(key.to_string_lossy().as_ref(), "relative-key.json"),
            ),
        ] {
            let path = directory.join(format!("{cloud}.toml"));
            fs::write(&path, &text).unwrap();
            let error = load_native_cloud_scope(cloud, &path, &hex::encode(Sha256::digest(&text)))
                .unwrap_err();
            assert!(!error.contains("secret-canary"));
        }

        fs::write(
            &key,
            r#"{"type":"authorized_user","refresh_token":"secret-adc-canary"}"#,
        )
        .unwrap();
        let path = directory.join("gcp.toml");
        fs::write(&path, &gcp).unwrap();
        let error =
            load_native_cloud_scope("gcp", &path, &hex::encode(Sha256::digest(&gcp))).unwrap_err();
        assert!(error.contains("service-account"));
        assert!(!error.contains("secret-adc-canary"));
        fs::remove_dir_all(directory).unwrap();
    }
}
