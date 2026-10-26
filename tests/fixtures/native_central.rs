use outrider::config::{CentralConfig, ConfigPatch};
use serde_json::{Value, json};
use std::{collections::BTreeMap, ffi::OsString, fs, path::Path};

/// Emit actual authored central and native GARM files from a normalized test case.
pub(crate) fn serialize_native_central(
    config: &Value,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut source = config.clone();
    omit_absent_settings(&mut source);
    let bindings = source
        .as_object_mut()
        .ok_or("configuration object missing")?
        .remove("provider_bindings")
        .unwrap_or_else(|| json!([]));
    let bindings = bindings.as_array().ok_or("provider binding list missing")?;
    if !bindings.is_empty() {
        let database = config["control"]["database_path"]
            .as_str()
            .ok_or("fixture database path missing")?;
        let garm_path = Path::new(database).with_extension("garm.toml");
        let registrations = bindings.iter().map(|binding| json!({
            "name": binding["id"], "provider_type":"external", "external": {
                "interface_version":"v0.1.0", "provider_executable":binding["executable_path"],
                "config_file":binding["configuration_path"], "environment_variables":binding["inherited_environment"],
            }
        })).collect::<Vec<_>>();
        fs::write(
            &garm_path,
            toml_edit::ser::to_string_pretty(&json!({"provider":registrations}))?,
        )?;
        source["garm"] = json!({"config_file":garm_path});
        source["providers"] = Value::Array(bindings.iter().map(|binding| json!({
            "name":binding["id"],"cloud":binding["cloud"],"executable_sha256":binding["executable_sha256"],
            "configuration_sha256":binding["configuration_sha256"],"source_revision":binding["source_revision"],
            "expected_version":binding["expected_version"],"supported_os":binding["supported_os"],
            "supported_architectures":binding["supported_architectures"],"purchasing_capabilities":binding["purchasing_capabilities"],
        })).collect());
    }

    for (field, derived, network_fields) in [
        (
            "aws_catalog_bindings",
            &["region", "credential_reference"][..],
            &["subnet_id"][..],
        ),
        (
            "azure_catalog_bindings",
            &[
                "account_id",
                "subscription_id",
                "pricing_location",
                "credential_reference",
            ][..],
            &["location", "vnet_subnet_id"][..],
        ),
        (
            "gcp_catalog_bindings",
            &[
                "project_id",
                "zone",
                "pricing_location",
                "credential_reference",
            ][..],
            &[][..],
        ),
    ] {
        for catalog in source
            .get_mut(field)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let fields = catalog.as_object_mut().ok_or("catalog object missing")?;
            for field in derived {
                fields.remove(*field);
            }
            for network in fields
                .get_mut("locations")
                .and_then(Value::as_object_mut)
                .into_iter()
                .flat_map(|networks| networks.values_mut())
            {
                for field in network_fields {
                    network
                        .as_object_mut()
                        .ok_or("network object missing")?
                        .remove(*field);
                }
            }
        }
    }

    Ok(toml_edit::ser::to_string_pretty(&source)?)
}

/// Supply the explicitly declared process variables used by fixture children.
pub(crate) fn load_fixture_central(path: &Path) -> Result<ConfigPatch, Box<dyn std::error::Error>> {
    let source = CentralConfig::from_toml(&fs::read_to_string(path)?)?;
    let mut environment = std::env::vars_os().collect::<BTreeMap<_, _>>();
    if let Some(garm) = &source.garm {
        let garm: Value = toml_edit::de::from_str(&fs::read_to_string(&garm.config_file)?)?;
        for provider in garm["provider"].as_array().into_iter().flatten() {
            for name in provider["external"]["environment_variables"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                environment
                    .entry(OsString::from(name))
                    .or_insert_with(|| OsString::from("fixture-process-value"));
            }
        }
    }

    Ok(source.load_native_configuration(&environment)?)
}

fn omit_absent_settings(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|_, value| !value.is_null());
            for value in fields.values_mut() {
                omit_absent_settings(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                omit_absent_settings(value);
            }
        }
        _ => {}
    }
}
