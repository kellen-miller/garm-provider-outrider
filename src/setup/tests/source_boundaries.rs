use super::ProtectedDirectory;
use crate::config::{CentralConfig, ConfigPatch};
use crate::providers::{configuration::load_native_cloud_scope, load_native_garm_providers};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs};

#[test]
fn authentication_and_authored_sources_stay_out_of_policy_snapshots() {
    let directory = ProtectedDirectory::new();
    let native = directory.file("native-source-canary.toml", "# existing native registry\n");
    let text = format!(
        "version = 1\nwebhook_listen_address = '127.0.0.1:8080'\n[authentication]\ngithub_app_id = 17\ngithub_installation_id = 42\ngithub_private_key_file = '/protected/private-key-canary.pem'\ngarm_credentials_file = '/protected/admin-credentials-canary.json'\n[garm]\nconfig_file = {:?}\n",
        native.to_str().unwrap()
    );
    let patch = CentralConfig::from_toml(&text)
        .unwrap()
        .load_native_configuration(&BTreeMap::new())
        .unwrap();
    let snapshot = serde_json::to_string(&patch).unwrap();
    for forbidden in [
        "authentication",
        "github_app_id",
        "github_installation_id",
        "private-key-canary",
        "admin-credentials-canary",
        "webhook_listen_address",
        "127.0.0.1:8080",
        "config_file",
        "source_revision",
        "native-source-canary",
    ] {
        assert!(
            !snapshot.contains(forbidden),
            "snapshot contains {forbidden}"
        );
    }
}

#[test]
fn repository_overlays_cannot_select_authority_listeners_or_metadata() {
    for table in [
        "authentication",
        "garm",
        "providers",
        "ingress",
        "control",
        "admission",
        "provider_bindings",
        "aws_catalog_bindings",
        "azure_catalog_bindings",
        "gcp_catalog_bindings",
        "effective_prices",
        "limits",
    ] {
        let text = format!("version = 1\n[{table}]\n");
        assert!(ConfigPatch::from_toml(&text).is_err(), "accepted [{table}]");
    }

    for field in [
        "webhook_listen_address",
        "github_private_key_file",
        "garm_credentials_file",
        "controller_id",
        "source_revision",
        "executable_sha256",
    ] {
        let text = format!("version = 1\n{field} = 'operator-selected'\n");
        assert!(ConfigPatch::from_toml(&text).is_err(), "accepted {field}");
    }
}

#[test]
fn malformed_native_toml_errors_never_include_secret_source_lines() {
    let directory = ProtectedDirectory::new();
    let contents = "secret_access_key = 'native-secret-canary'\nmalformed = [\n";
    let registry = directory.file("garm.toml", contents);
    let error =
        load_native_garm_providers(&registry, &["aws".into()], "outrider", &BTreeMap::new())
            .unwrap_err();
    assert_eq!(error.to_string(), "native GARM configuration is invalid");
    for (cloud, label) in [("aws", "AWS"), ("azure", "Azure"), ("gcp", "GCP")] {
        let path = directory.file(&format!("{cloud}.toml"), contents);
        let digest = hex::encode(Sha256::digest(fs::read(&path).unwrap()));
        let error = load_native_cloud_scope(cloud, &path, &digest).unwrap_err();
        assert!(!error.contains("native-secret-canary"));
        assert!(!error.contains("secret_access_key"));
        assert_eq!(error, format!("native {label} configuration is invalid"));
    }
}
