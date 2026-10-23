use std::process::Command;

use outrider::bootstrap::{self, BootstrapPlatform, BootstrapProvider};

use serde_yaml_ng::Value as YamlValue;

use super::garm_renderer::{
    TestDirectory, common_render, hook_script, linux_instance, set_platform, success, target,
};

#[test]
fn provider_linux_option_mapping() {
    let mut linux = linux_instance().unwrap();
    linux.user_data_options.extra_packages = vec!["jq".to_owned()];
    linux.user_data_options.disable_updates_on_boot = true;
    let aws_hook = bootstrap::generate_provider_hook(
        &linux,
        target(BootstrapPlatform::LinuxAmd64),
        BootstrapProvider::Aws,
    )
    .unwrap();
    assert!(aws_hook.requires_cloud_init);
    assert!(aws_hook.user_data_options.disable_updates_on_boot);
    for package in ["curl", "tar", "sudo", "coreutils", "jq"] {
        assert!(
            aws_hook
                .user_data_options
                .extra_packages
                .contains(&package.to_owned())
        );
    }
    let aws_specs = aws_hook.extra_specs_json();
    assert_eq!(aws_specs["disable_updates"], true);
    assert_eq!(aws_specs["extra_packages"].as_array().unwrap().len(), 5);
    let aws_render = common_render(
        &linux,
        BootstrapProvider::Aws,
        BootstrapPlatform::LinuxAmd64,
    )
    .unwrap();
    let aws_config: YamlValue = serde_yaml_ng::from_str(
        aws_render
            .cloud_config
            .trim_start_matches("#cloud-config\n"),
    )
    .unwrap();
    assert_eq!(aws_config["package_upgrade"], false);
    assert!(
        aws_config["packages"]
            .as_sequence()
            .unwrap()
            .contains(&YamlValue::String("jq".to_owned()))
    );

    let azure_hook = bootstrap::generate_provider_hook(
        &linux,
        target(BootstrapPlatform::LinuxAmd64),
        BootstrapProvider::Azure,
    )
    .unwrap();
    let azure_specs = azure_hook.extra_specs_json();
    assert_eq!(azure_specs["disable_updates"], true);
    assert_eq!(azure_specs["enable_boot_debug"], false);
    assert_eq!(azure_specs["extra_packages"].as_array().unwrap().len(), 5);
    let azure_render = common_render(
        &linux,
        BootstrapProvider::Azure,
        BootstrapPlatform::LinuxAmd64,
    )
    .unwrap();
    let azure_config: YamlValue = serde_yaml_ng::from_str(
        azure_render
            .cloud_config
            .trim_start_matches("#cloud-config\n"),
    )
    .unwrap();
    assert_eq!(azure_config["package_upgrade"], false);
    assert!(
        azure_config["packages"]
            .as_sequence()
            .unwrap()
            .contains(&YamlValue::String("jq".to_owned()))
    );

    let gcp_hook = bootstrap::generate_provider_hook(
        &linux,
        target(BootstrapPlatform::LinuxAmd64),
        BootstrapProvider::Gcp,
    )
    .unwrap();
    let gcp_specs = gcp_hook.extra_specs_json();
    assert!(gcp_specs.get("extra_packages").is_none());
    assert_eq!(gcp_specs["disable_updates"], true);
    assert!(
        gcp_hook
            .user_data_options
            .extra_packages
            .contains(&"jq".to_owned())
    );
    let gcp_render = common_render(
        &linux,
        BootstrapProvider::Gcp,
        BootstrapPlatform::LinuxAmd64,
    )
    .unwrap();
    let gcp_config: YamlValue = serde_yaml_ng::from_str(
        gcp_render
            .cloud_config
            .trim_start_matches("#cloud-config\n"),
    )
    .unwrap();
    assert_eq!(gcp_config["package_upgrade"], false);
    assert!(
        gcp_config["packages"]
            .as_sequence()
            .unwrap()
            .contains(&YamlValue::String("jq".to_owned()))
    );
}

#[test]
fn actual_common_rendering() {
    let mut linux = linux_instance().unwrap();
    linux.name = "runner \"quoted\" {{ .Name }} café 🧪 100%".to_owned();
    linux.ssh_keys = (0..3)
        .map(|index| format!("ssh-ed25519 AAAA-{{{{ .Key{index} }}}} café \"\\"))
        .collect();
    linux.user_data_options.extra_packages = (0..8).map(|index| format!("jq-{index}")).collect();
    linux.ca_cert_bundle = Some(include_bytes!("../fixtures/bootstrap-render/ca.pem").to_vec());
    linux.user_data_options.disable_updates_on_boot = true;
    let linux_hook = bootstrap::generate_provider_hook(
        &linux,
        target(BootstrapPlatform::LinuxAmd64),
        BootstrapProvider::Aws,
    )
    .unwrap();
    assert!(linux_hook.requires_cloud_init);
    let linux_render = common_render(
        &linux,
        BootstrapProvider::Aws,
        BootstrapPlatform::LinuxAmd64,
    )
    .unwrap();
    assert_eq!(
        linux_render.install_script,
        hook_script(&linux_hook.extra_specs_json()).unwrap()
    );
    assert!(linux_render.cloud_config.starts_with("#cloud-config\n"));
    assert!(!linux_render.install_script.starts_with("#cloud-config"));
    assert!(linux_render.cloud_config.len() <= linux_hook.wrapped_payload_bound_bytes);
    for provider in [BootstrapProvider::Azure, BootstrapProvider::Gcp] {
        let hook = bootstrap::generate_provider_hook(
            &linux,
            target(BootstrapPlatform::LinuxAmd64),
            provider,
        )
        .unwrap();
        if provider == BootstrapProvider::Azure {
            let specs = hook.extra_specs_json();
            assert_eq!(specs["disable_updates"], true);
            assert_eq!(specs["enable_boot_debug"], false);
            assert_eq!(specs["extra_packages"].as_array().unwrap().len(), 13);
        }
        let render = common_render(&linux, provider, BootstrapPlatform::LinuxAmd64).unwrap();
        assert!(render.cloud_config.len() <= hook.wrapped_payload_bound_bytes);
        assert!(render.cloud_config.len() <= hook.largest_metadata_value_bound_bytes);
        assert!(hook.wrapped_payload_bound_bytes <= hook.transport_limit_bytes);
    }
    let directory = TestDirectory::new().unwrap();
    let shell_path = directory
        .write("common-linux.sh", &linux_render.install_script)
        .unwrap();
    success(
        Command::new("bash").arg("-n").arg(shell_path),
        "bash",
        "install Bash (present on Linux and macOS)",
    )
    .unwrap();
    let yaml_path = directory
        .write("common-cloud-config.yaml", &linux_render.cloud_config)
        .unwrap();
    success(
        Command::new("cloud-init").args(["schema", "--config-file"]).arg(yaml_path),
        "cloud-init 26.2",
        "install cloud-init 26.2; see https://cloudinit.readthedocs.io/en/26.2/reference/cli.html#schema",
    )
    .unwrap();
}

#[test]
fn actual_common_windows_rendering() {
    let directory = TestDirectory::new().unwrap();
    let mut windows = linux_instance().unwrap();
    set_platform(&mut windows, "windows", "amd64", "win", "x64");
    let windows_hook = bootstrap::generate_provider_hook(
        &windows,
        target(BootstrapPlatform::WindowsAmd64),
        BootstrapProvider::Azure,
    )
    .unwrap();
    assert!(!windows_hook.requires_cloud_init);
    assert!(
        windows_hook.extra_specs_json()["extra_packages"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    assert_eq!(windows_hook.extra_specs_json()["disable_updates"], false);
    let windows_render = common_render(
        &windows,
        BootstrapProvider::Azure,
        BootstrapPlatform::WindowsAmd64,
    )
    .unwrap();
    assert_eq!(windows_render.install_script, windows_render.cloud_config);
    assert!(windows_render.install_script.len() <= windows_hook.wrapped_payload_bound_bytes);
    assert!(windows_render.install_script.len() <= windows_hook.largest_metadata_value_bound_bytes);
    for provider in [BootstrapProvider::Aws, BootstrapProvider::Gcp] {
        let hook = bootstrap::generate_provider_hook(
            &windows,
            target(BootstrapPlatform::WindowsAmd64),
            provider,
        )
        .unwrap();
        let render = common_render(&windows, provider, BootstrapPlatform::WindowsAmd64).unwrap();
        assert!(render.install_script.len() <= hook.wrapped_payload_bound_bytes);
        assert!(render.install_script.len() <= hook.largest_metadata_value_bound_bytes);
        assert!(hook.wrapped_payload_bound_bytes <= hook.transport_limit_bytes);
    }
    let powershell_path = directory
        .write("common-windows.ps1", &windows_render.install_script)
        .unwrap();
    let parser = format!(
        "$tokens = $null; $errors = $null; [System.Management.Automation.Language.Parser]::ParseFile('{}', [ref]$tokens, [ref]$errors) > $null; if ($errors.Count) {{ throw $errors[0] }}",
        powershell_path.display()
    );
    success(
        Command::new("pwsh").args(["-NoProfile", "-NonInteractive", "-Command", &parser]),
        "PowerShell 7.6.7 (pwsh)",
        "install PowerShell 7.6.7; see https://learn.microsoft.com/powershell/scripting/install/installing-powershell",
    )
    .unwrap();
}
