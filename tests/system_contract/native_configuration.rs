use super::*;
use outrider::config::CentralConfig;

#[test]
fn native_sources_own_paths_scopes_and_environment_without_retaining_secrets()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = JournalFixture::new();
    let binding = allocation_binding(&fixture, "normal");
    let mut config = allocation_central(&binding, "newAMI");
    config
        .control
        .as_mut()
        .ok_or("control missing")?
        .database_path = fixture
        .directory
        .join("native.sqlite3")
        .display()
        .to_string();
    let authored = serialize_native_central(&serde_json::to_value(&config)?)?;
    let mut garm: Value = toml_edit::de::from_str(&fs::read_to_string(
        fixture.directory.join("native.garm.toml"),
    )?)?;
    garm["jwt_auth"] = json!({"secret":"garm-jwt-secret-canary"});
    let alternate = fixture.directory.join("native-authority.toml");
    fs::copy(&binding.configuration_path, &alternate)?;
    garm["provider"][0]["external"]["config_file"] = json!(alternate);
    fs::write(
        fixture.directory.join("native.garm.toml"),
        toml_edit::ser::to_string_pretty(&garm)?,
    )?;
    let environment = binding
        .inherited_environment
        .iter()
        .map(|name| {
            (
                OsString::from(name),
                OsString::from("environment-secret-canary"),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let resolved = CentralConfig::from_toml(&authored)?.load_native_configuration(&environment)?;
    let imported = resolved
        .provider_bindings
        .as_ref()
        .ok_or("bindings missing")?
        .first()
        .ok_or("provider missing")?;
    assert_eq!(imported.configuration_path, alternate);
    assert_eq!(
        imported.inherited_environment,
        binding.inherited_environment
    );
    let snapshot = serde_json::to_string(&resolved)?;
    for secret in [
        "garm-jwt-secret-canary",
        "environment-secret-canary",
        "secret-test-only-not-a-real-key",
    ] {
        assert!(!snapshot.contains(secret));
    }
    assert!(!snapshot.contains("garm.toml"));
    assert!(
        ProviderRegistry::new(
            resolved
                .provider_bindings
                .clone()
                .ok_or("bindings missing")?,
            BTreeMap::new(),
            ProcessLimits::default()
        )
        .is_err()
    );
    ProviderRegistry::new(
        resolved
            .provider_bindings
            .clone()
            .ok_or("bindings missing")?,
        environment.clone(),
        ProcessLimits::default(),
    )?;

    let changed = fs::read_to_string(&alternate)?
        .replace("us-east-1", "eu-west-1")
        .replace("subnet-123", "subnet-native");
    fs::write(&alternate, &changed)?;
    let mut source: Value = toml_edit::de::from_str(&authored)?;
    source["providers"][0]["configuration_sha256"] =
        json!(hex::encode(Sha256::digest(changed.as_bytes())));
    let changed_source = toml_edit::ser::to_string_pretty(&source)?;
    let resolved =
        CentralConfig::from_toml(&changed_source)?.load_native_configuration(&environment)?;
    let catalog = &resolved
        .aws_catalog_bindings
        .as_ref()
        .ok_or("AWS catalog missing")?[0];
    assert_eq!(catalog.region, "eu-west-1");
    assert_eq!(catalog.locations["us-east-1"].subnet_id, "subnet-native");
    assert_eq!(
        resolved
            .provider_bindings
            .as_ref()
            .ok_or("bindings missing")?[0]
            .location,
        "eu-west-1"
    );

    fs::write(&alternate, "region = 'native-secret-canary'\ninvalid = [")?;
    source["providers"][0]["configuration_sha256"] = json!(sha256_file(&alternate)?);
    let invalid = toml_edit::ser::to_string_pretty(&source)?;
    let error = CentralConfig::from_toml(&invalid)?
        .load_native_configuration(&environment)
        .err()
        .ok_or("malformed native source accepted")?;
    assert!(!error.to_string().contains("native-secret-canary"));
    assert!(
        error
            .to_string()
            .contains("native AWS configuration is invalid")
    );
    Ok(())
}

#[test]
fn authored_schema_and_parser_reject_duplicate_native_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let source: Value = toml_edit::de::from_str(include_str!("../../examples/config.toml"))?;
    let schema = outrider::config::json_schema();
    let validator = jsonschema::validator_for(&schema)?;
    assert!(validator.is_valid(&source));
    CentralConfig::from_toml(include_str!("../../examples/config-minimal.toml"))?;
    assert!(schema["properties"].get("provider_bindings").is_none());
    assert!(schema["$defs"].get("ProviderBinding").is_none());
    for (table, field, value) in [
        (
            "providers",
            "configuration_path",
            json!("/duplicated/native.toml"),
        ),
        (
            "providers",
            "executable_path",
            json!("/duplicated/provider"),
        ),
        ("providers", "inherited_environment", json!(["SECRET"])),
        ("aws_catalog_bindings", "region", json!("eu-west-1")),
        (
            "azure_catalog_bindings",
            "subscription_id",
            json!("duplicate-subscription"),
        ),
        (
            "gcp_catalog_bindings",
            "project_id",
            json!("duplicate-project"),
        ),
    ] {
        let mut invalid = source.clone();
        invalid[table][0][field] = value;
        assert!(!validator.is_valid(&invalid));
        assert!(CentralConfig::from_toml(&toml_edit::ser::to_string_pretty(&invalid)?).is_err());
    }
    for key in ["garm", "providers", "provider_bindings"] {
        let mut repository = json!({"version":1});
        repository[key] = source.get(key).cloned().unwrap_or_else(|| json!([]));
        if key == "provider_bindings" {
            assert!(
                CentralConfig::from_toml(&toml_edit::ser::to_string_pretty(&repository)?).is_err()
            );
        } else {
            assert!(
                ConfigPatch::from_toml(&toml_edit::ser::to_string_pretty(&repository)?).is_err()
            );
        }
    }
    Ok(())
}
