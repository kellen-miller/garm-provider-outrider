use outrider::bootstrap::{self, BootstrapPlatform, BootstrapProvider};

use super::garm_renderer::{
    common_render, delivered_script, hook_script, linux_instance, set_platform, target,
};

#[test]
fn linux_token_no_default_labels() {
    let mut instance = linux_instance().unwrap();
    instance.jit_config_enabled = false;
    let script = delivered_script(
        &instance,
        BootstrapProvider::Aws,
        BootstrapPlatform::LinuxAmd64,
    )
    .unwrap();
    assert!(script.contains("runner-registration-token/"));
    assert!(script.contains("--no-default-labels --ephemeral"));
    assert!(script.contains("--labels \"$RUNNER_LABELS\""));
    assert!(!script.contains("--runnergroup"));
}

#[test]
fn linux_jit_exact_labels() {
    let instance = linux_instance().unwrap();
    let script = delivered_script(
        &instance,
        BootstrapProvider::Aws,
        BootstrapPlatform::LinuxAmd64,
    )
    .unwrap();
    assert!(script.contains("${METADATA_URL%/}/credentials/${item}"));
    assert!(script.contains("/systemd/unit-file?runAsUser=runner"));
    assert!(script.contains("cp bin/runsvc.sh ./runsvc.sh"));
    let jit_branch = script
        .split_once("if [[ \"$JIT_ENABLED\" == true ]]; then")
        .unwrap()
        .1
        .split_once("else")
        .unwrap()
        .0;
    assert!(!jit_branch.contains(" --labels "));
    assert!(!script.contains("--runnergroup"));
    assert!(script.contains("JIT_ENABLED=true"));
}

#[test]
fn provider_hook_literal_round_trip() {
    let mut instance = linux_instance().unwrap();
    instance.name = "runner \"quoted\" {{ .Name }} café 🧪 100%".to_owned();
    instance.ssh_keys = vec!["ssh-ed25519 AAAA literal {{ .Field }} café".to_owned()];
    let expected_script = delivered_script(
        &instance,
        BootstrapProvider::Aws,
        BootstrapPlatform::LinuxAmd64,
    )
    .unwrap();
    let hook = bootstrap::generate_provider_hook(
        &instance,
        target(BootstrapPlatform::LinuxAmd64),
        BootstrapProvider::Aws,
    )
    .unwrap();
    let specs = hook.extra_specs_json();
    let round_trip = hook_script(&specs).unwrap();
    assert_eq!(round_trip, expected_script);
    assert_eq!(
        specs["extra_context"]["outrider_sha256"],
        instance.tools[0].sha256_checksum.as_deref().unwrap()
    );
    assert_eq!(specs["disable_updates"], false);
    assert_eq!(specs["enable_boot_debug"], false);
    assert!(specs["extra_packages"].is_array());
    assert!(specs.get("snapshot_id").is_none());
    assert!(!format!("{hook:?}").contains(&instance.name));
    assert!(!format!("{hook:?}").contains("runner-install"));
}

#[test]
fn wrapped_payload_limit() {
    let mut oversized = linux_instance().unwrap();
    oversized
        .user_data_options
        .extra_packages
        .push("x".repeat(4096));
    assert!(
        bootstrap::generate_provider_hook(
            &oversized,
            target(BootstrapPlatform::LinuxAmd64),
            BootstrapProvider::Aws,
        )
        .is_err()
    );

    let mut oversized_ssh = linux_instance().unwrap();
    oversized_ssh
        .ssh_keys
        .push(format!("ssh-ed25519 {}", "x".repeat(4096)));
    assert!(
        bootstrap::generate_provider_hook(
            &oversized_ssh,
            target(BootstrapPlatform::LinuxAmd64),
            BootstrapProvider::Aws,
        )
        .is_err()
    );

    let mut oversized_ca = linux_instance().unwrap();
    oversized_ca.ca_cert_bundle = Some(
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            "x".repeat(4096)
        )
        .into_bytes(),
    );
    assert!(
        bootstrap::generate_provider_hook(
            &oversized_ca,
            target(BootstrapPlatform::LinuxAmd64),
            BootstrapProvider::Aws,
        )
        .is_err()
    );

    let mut oversized_script = linux_instance().unwrap();
    oversized_script.name = "r".repeat(16_384);
    assert!(
        bootstrap::generate_provider_hook(
            &oversized_script,
            target(BootstrapPlatform::LinuxAmd64),
            BootstrapProvider::Aws,
        )
        .is_err()
    );

    let mut windows = linux_instance().unwrap();
    set_platform(&mut windows, "windows", "amd64", "win", "x64");
    let windows_hook = bootstrap::generate_provider_hook(
        &windows,
        target(BootstrapPlatform::WindowsAmd64),
        BootstrapProvider::Aws,
    )
    .unwrap();
    let windows_render = common_render(
        &windows,
        BootstrapProvider::Aws,
        BootstrapPlatform::WindowsAmd64,
    )
    .unwrap();
    assert!(
        windows_hook.wrapped_payload_bound_bytes
            >= windows_render.install_script.len() + "<powershell></powershell>".len() + 128
    );
    assert!(windows_hook.wrapped_payload_bound_bytes <= windows_hook.transport_limit_bytes);

    let mut azure_windows_oversized = linux_instance().unwrap();
    set_platform(
        &mut azure_windows_oversized,
        "windows",
        "amd64",
        "win",
        "x64",
    );
    azure_windows_oversized.name = "r".repeat(66_000);
    assert!(
        bootstrap::generate_provider_hook(
            &azure_windows_oversized,
            target(BootstrapPlatform::WindowsAmd64),
            BootstrapProvider::Azure,
        )
        .is_err()
    );

    let mut gcp_windows_oversized = linux_instance().unwrap();
    set_platform(&mut gcp_windows_oversized, "windows", "amd64", "win", "x64");
    gcp_windows_oversized.name = "r".repeat(263_000);
    assert!(
        bootstrap::generate_provider_hook(
            &gcp_windows_oversized,
            target(BootstrapPlatform::WindowsAmd64),
            BootstrapProvider::Gcp,
        )
        .is_err()
    );

    let mut mac = linux_instance().unwrap();
    set_platform(&mut mac, "macos", "arm64", "osx", "arm64");
    assert!(
        bootstrap::generate_provider_hook(
            &mac,
            target(BootstrapPlatform::MacosArm64),
            BootstrapProvider::Aws,
        )
        .is_err()
    );
}
