use crate::setup::{SetupError, merge_registration, registration};
use std::path::Path;
use toml_edit::DocumentMut;

#[test]
fn registration_adds_only_own_entry_and_repeats_without_changes() {
    let original = "# operator registry\n[apiserver]\nbind = '0.0.0.0'\nport = 9997\n\n[[provider]]\nname = 'aws'\nprovider_type = 'external'\n[provider.external]\nprovider_executable = '/native/aws'\nconfig_file = '/native/aws.toml'\n";
    let mut document = original.parse::<DocumentMut>().unwrap();
    let native_before = document["provider"][0].as_table().unwrap().to_string();
    let api_before = document["apiserver"].as_table().unwrap().to_string();
    let expected = registration(
        "outrider",
        Path::new("/etc/outrider/config.toml"),
        Path::new("/opt/outrider"),
    )
    .unwrap();
    merge_registration(&mut document, "outrider", &expected).unwrap();
    let once = document.to_string();
    assert!(once.contains("# operator registry"));
    assert_eq!(
        document["provider"][0].as_table().unwrap().to_string(),
        native_before
    );
    assert_eq!(
        document["apiserver"].as_table().unwrap().to_string(),
        api_before
    );
    merge_registration(&mut document, "outrider", &expected).unwrap();
    assert_eq!(document.to_string(), once);
    let own = &document["provider"][1];
    assert_eq!(own["disable_jit_config"].as_bool(), Some(true));
    assert_eq!(
        own["external"]["interface_version"].as_str(),
        Some("v0.1.0")
    );
    assert_eq!(
        own["external"]["environment_variables"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn equivalent_operator_registration_is_semantically_idempotent() {
    let original = "# operator formatting stays\n[[provider]]\nprovider_type = 'external' # already external\nname = 'outrider'\ndisable_jit_config = true\n[provider.external]\nconfig_file = '/etc/outrider/config.toml'\nprovider_executable = '/opt/outrider/bin/outrider'\nenvironment_variables = [ ]\ninterface_version = 'v0.1.0'\n";
    let mut document = original.parse::<DocumentMut>().unwrap();
    let expected = registration(
        "outrider",
        Path::new("/etc/outrider/config.toml"),
        Path::new("/opt/outrider"),
    )
    .unwrap();
    merge_registration(&mut document, "outrider", &expected).unwrap();
    assert_eq!(document.to_string(), original);
}

#[test]
fn conflicting_or_duplicate_registration_is_preserved_and_rejected() {
    let expected = registration(
        "outrider",
        Path::new("/etc/outrider/config.toml"),
        Path::new("/opt/outrider"),
    )
    .unwrap();
    for original in [
        expected.replace("disable_jit_config = true", "disable_jit_config = false"),
        format!("{expected}\n{expected}"),
    ] {
        let mut document = original.parse::<DocumentMut>().unwrap();
        assert!(matches!(
            merge_registration(&mut document, "outrider", &expected),
            Err(SetupError::Registration)
        ));
        assert_eq!(document.to_string(), original);
    }
}
