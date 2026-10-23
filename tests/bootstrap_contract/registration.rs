use std::{io, path::Path, process::Command};

use outrider::bootstrap::{self, BootstrapPlatform, BootstrapProvider};

use secrecy::ExposeSecret;

use super::garm_renderer::{
    TestDirectory, common_render, delivered_script, linux_instance, linux_script, set_platform,
    success, target,
};

#[test]
fn windows_repository_registration() {
    let mut instance = linux_instance().unwrap();
    set_platform(&mut instance, "windows", "amd64", "win", "x64");
    instance.jit_config_enabled = false;
    instance.labels = vec!["%7Bworkflow%7D".to_owned()];
    let script = delivered_script(
        &instance,
        BootstrapProvider::Aws,
        BootstrapPlatform::WindowsAmd64,
    )
    .unwrap();
    assert!(script.contains("$false"));
    assert!(script.contains("runner-registration-token/"));
    assert!(script.contains("'--no-default-labels', '--ephemeral', '--runasservice'"));
    assert!(script.contains("'--windowslogonaccount', 'NT AUTHORITY\\SYSTEM'"));
    assert!(script.contains("'--labels', $RunnerLabels"));
    assert!(script.contains("Get-ChildItem -Path $RunnerDirectory -Recurse -File | Unblock-File"));
    assert!(script.contains("'bin\\Runner.Listener.exe') configure @configArguments"));
    assert!(!script.contains("--runnergroup"));
    assert!(!script.contains("svc.cmd"));
    assert!(!script.contains("config.cmd"));
    assert!(script.contains("Add-Type -AssemblyName System.Security"));
    assert!(script.contains("runner .runner file has no positive agent ID"));

    let directory = TestDirectory::new().unwrap();
    let windows_path = directory.write("labels.ps1", &script).unwrap();
    let check = format!(
        "$tokens = $null; $errors = $null; $ast = [System.Management.Automation.Language.Parser]::ParseFile('{}', [ref]$tokens, [ref]$errors); if ($errors.Count) {{ exit 2 }}; $allowed = @('RunnerLabels','configArguments'); $ast.FindAll({{ param($node) $node -is [System.Management.Automation.Language.AssignmentStatementAst] }}, $true) | ForEach-Object {{ $name = $_.Left.Extent.Text.TrimStart('$'); if ($allowed -contains $name) {{ Invoke-Expression $_.Extent.Text }} }}; if ($configArguments[8] -cne '%7Bworkflow%7D') {{ exit 3 }}",
        windows_path.display()
    );
    success(
        Command::new("pwsh").env("XDG_CACHE_HOME", windows_path.with_extension("pwsh-cache")).args(["-NoProfile", "-NonInteractive", "-Command", &check]),
        "PowerShell 7.6.7 (pwsh)",
        "install PowerShell 7.6.7; see https://learn.microsoft.com/powershell/scripting/install/installing-powershell",
    )
    .unwrap();
}

#[test]
fn macos_direct_credentials() {
    let mut instance = linux_instance().unwrap();
    set_platform(&mut instance, "macos", "arm64", "osx", "arm64");
    let keys = vec![
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIexample first-key".to_owned(),
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIexample second'key".to_owned(),
    ];
    instance.ssh_keys = keys.clone();
    let payload =
        bootstrap::generate_macos(&instance, target(BootstrapPlatform::MacosArm64)).unwrap();
    let script = payload.expose_secret();
    assert!(script.starts_with("#!/usr/bin/env bash"));
    assert!(script.contains("${METADATA_URL%/}/credentials/${item}"));
    assert!(script.contains("com.outrider.runner.plist"));
    assert!(!script.contains("/systemd/"));
    assert!(!script.contains("system/service-name"));
    assert!(!script.contains("installdependencies.sh"));
    assert!(!script.contains("/instance-metadata"));
    assert!(script.contains("=~ ^[1-9][0-9]*$ ]] || fail"));
    let ssh_commands = script
        .lines()
        .filter(|line| line.starts_with("printf '%s\\n'"))
        .map(|line| line.replace(">> /Users/runner/.ssh/authorized_keys", ">> /dev/stdout"))
        .collect::<Vec<_>>()
        .join("\n");
    let output = success(
        Command::new("/bin/bash")
            .arg("-c")
            .arg(format!("set -Eeuo pipefail\n{ssh_commands}\n")),
        "/bin/bash",
        "install Bash (present on Linux and macOS)",
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        keys.join("\n") + "\n"
    );
    let mut unqualified = target(BootstrapPlatform::MacosArm64);
    unqualified.macos_user_data_enabled = false;
    assert!(bootstrap::generate_macos(&instance, unqualified).is_err());
}

#[test]
fn checksum_ca_callback() {
    let mut instance = linux_instance().unwrap();
    instance.ca_cert_bundle = Some(include_bytes!("../fixtures/bootstrap-render/ca.pem").to_vec());
    let render = common_render(
        &instance,
        BootstrapProvider::Aws,
        BootstrapPlatform::LinuxAmd64,
    )
    .unwrap();
    let script = linux_script(&render.cloud_config).unwrap();
    assert!(script.contains("sha256sum --check --status"));
    assert!(script.contains("EXPECTED_SHA256"));
    assert!(render.cloud_config.contains("ca-certs:"));
    assert!(script.contains("/system-info/"));
    assert!(script.contains("runner successfully installed"));
    assert!(script.contains("agent_id"));
    assert!(script.contains("type == \"number\" and . > 0 and . == floor"));
    assert!(script.contains("\"status\":\"failed\""));

    let mut arm64 = linux_instance().unwrap();
    set_platform(&mut arm64, "linux", "arm64", "linux", "arm64");
    let arm64_script = delivered_script(
        &arm64,
        BootstrapProvider::Aws,
        BootstrapPlatform::LinuxArm64,
    )
    .unwrap();
    assert!(arm64_script.contains("EXPECTED_SHA256"));

    let mut mismatched = arm64;
    mismatched.tools[0].architecture = Some("x64".to_owned());
    assert!(
        bootstrap::generate_provider_hook(
            &mismatched,
            target(BootstrapPlatform::LinuxArm64),
            BootstrapProvider::Aws,
        )
        .is_err()
    );
}

#[test]
fn injection_and_secret_redaction() {
    let directory = TestDirectory::new().unwrap();
    let mut instance = linux_instance().unwrap();
    instance.tools[0].temp_download_token = None;
    let canary = directory.path.join("injection-canary");
    let malicious = format!(
        "literal ' \" $(touch {}) `id` ; $env:SECRET <&> @@DOWNLOAD_URL@@",
        canary.display()
    );
    instance.name = malicious.clone();
    instance.instance_token = "synthetic-callback-secret-canary".to_owned();
    let script = delivered_script(
        &instance,
        BootstrapProvider::Aws,
        BootstrapPlatform::LinuxAmd64,
    )
    .unwrap();
    assert!(script.contains("@@DOWNLOAD_URL@@"));
    assert!(!script.contains("set -x"));
    assert!(!script.contains("printf.*BEARER_TOKEN"));
    let hook = bootstrap::generate_provider_hook(
        &instance,
        target(BootstrapPlatform::LinuxAmd64),
        BootstrapProvider::Aws,
    )
    .unwrap();
    let debug = format!("{hook:?}");
    assert!(!debug.contains("synthetic-callback-secret-canary"));
    assert!(!debug.contains("synthetic-download-secret-canary"));
    let mut secret_instance = instance.clone();
    secret_instance.tools[0].temp_download_token =
        Some("synthetic-download-secret-canary".to_owned());
    let secret_hook = bootstrap::generate_provider_hook(
        &secret_instance,
        target(BootstrapPlatform::LinuxAmd64),
        BootstrapProvider::Aws,
    )
    .unwrap();
    assert!(!format!("{secret_hook:?}").contains("synthetic-download-secret-canary"));
    let mut mac_instance = instance.clone();
    set_platform(&mut mac_instance, "macos", "arm64", "osx", "arm64");
    let mac =
        bootstrap::generate_macos(&mac_instance, target(BootstrapPlatform::MacosArm64)).unwrap();
    assert!(
        mac.expose_secret()
            .contains(&format!("$(touch {})", canary.display()))
    );
    assert!(!format!("{mac:?}").contains("synthetic-callback-secret-canary"));
    let assignment = script
        .lines()
        .find(|line| line.starts_with("RUNNER_NAME="))
        .unwrap();
    let command = format!("{assignment}\ntest \"$RUNNER_NAME\" = \"$1\"\n");
    success(
        Command::new("/bin/bash")
            .arg("-c")
            .arg(command)
            .arg("bootstrap-test")
            .arg(&malicious),
        "/bin/bash",
        "install Bash (present on Linux and macOS)",
    )
    .unwrap();
    assert!(!canary.exists());

    assert_download_token_redaction(script.as_str()).unwrap();
    assert_download_token_redaction(mac.expose_secret()).unwrap();

    let mut windows_instance = instance.clone();
    set_platform(&mut windows_instance, "windows", "amd64", "win", "x64");
    windows_instance.jit_config_enabled = false;
    let windows = delivered_script(
        &windows_instance,
        BootstrapProvider::Aws,
        BootstrapPlatform::WindowsAmd64,
    )
    .unwrap();
    let windows_path = directory.write("injection.ps1", &windows).unwrap();
    let expected = format!("'{}'", malicious.replace('\'', "''"));
    let parser = format!(
        "$tokens = $null; $errors = $null; $ast = [System.Management.Automation.Language.Parser]::ParseFile('{}', [ref]$tokens, [ref]$errors); if ($errors.Count) {{ exit 2 }}; $allowed = @('CallbackUrl','MetadataUrl','BearerToken','RunnerName','DownloadUrl','DownloadToken','Filename','ExpectedSha256','RepositoryUrl','RunnerLabels','JitEnabled','RunnerDirectory'); $ast.FindAll({{ param($node) $node -is [System.Management.Automation.Language.AssignmentStatementAst] }}, $true) | ForEach-Object {{ $name = $_.Left.Extent.Text.TrimStart('$'); if ($allowed -contains $name) {{ Invoke-Expression $_.Extent.Text }} }}; if ($RunnerName -cne {expected}) {{ exit 3 }}; if ($JitEnabled -isnot [bool] -or $JitEnabled) {{ exit 4 }}",
        windows_path.display()
    );
    success(
        Command::new("pwsh").env("XDG_CACHE_HOME", windows_path.with_extension("pwsh-cache")).args(["-NoProfile", "-NonInteractive", "-Command", &parser]),
        "PowerShell 7.6.7 (pwsh)",
        "install PowerShell 7.6.7; see https://learn.microsoft.com/powershell/scripting/install/installing-powershell",
    )
    .unwrap();
}

fn assert_download_token_redaction(script: &str) -> Result<(), Box<dyn std::error::Error>> {
    let download_assignments = script
        .lines()
        .filter(|line| line.starts_with("DOWNLOAD_TOKEN=") || line.starts_with("DOWNLOAD_URL="))
        .collect::<Vec<_>>()
        .join("\n");
    let expected_download_url = script
        .lines()
        .find(|line| line.starts_with("DOWNLOAD_URL="))
        .ok_or_else(|| io::Error::other("download URL assignment is missing"))?
        .split_once('=')
        .ok_or_else(|| io::Error::other("download URL assignment is malformed"))?
        .1
        .trim_matches('\'');
    let setup_start = script
        .find("headers=(-H 'Accept: application/octet-stream')")
        .ok_or_else(|| io::Error::other("download setup is missing"))?;
    let setup_end = script[setup_start..]
        .find("\nprintf '%s  %s\\n'")
        .ok_or_else(|| io::Error::other("download setup end is missing"))?;
    let download_setup = &script[setup_start..setup_start + setup_end];
    let command = format!(
        "set -Eeuo pipefail\n{download_assignments}\narchive=/tmp/no-write\ncurl() {{ printf '%s\\n' \"$@\"; }}\n{download_setup}\n"
    );
    let output = success(
        Command::new("/bin/bash").arg("-c").arg(command),
        "/bin/bash",
        "install Bash (present on Linux and macOS)",
    )?;
    let arguments = String::from_utf8(output.stdout)?;
    assert!(arguments.contains("Accept: application/octet-stream"));
    assert!(!arguments.contains("Authorization: Bearer"));
    assert!(arguments.contains(expected_download_url));
    Ok(())
}

fn validate_windows_bootstrap(directory: &TestDirectory) -> Result<(), Box<dyn std::error::Error>> {
    let mut instance = linux_instance()?;
    set_platform(&mut instance, "windows", "amd64", "win", "x64");
    let ssh_keys = vec![
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIexample windows-key".to_owned(),
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIexample windows'key".to_owned(),
    ];
    instance.ssh_keys = ssh_keys.clone();
    instance.ca_cert_bundle =
        Some(b"-----BEGIN CERTIFICATE-----\\nZm9v\\n-----END CERTIFICATE-----\\n".to_vec());
    instance.user_data_options.disable_updates_on_boot = true;
    let windows = delivered_script(
        &instance,
        BootstrapProvider::Aws,
        BootstrapPlatform::WindowsAmd64,
    )?;
    let path = directory.write("windows.ps1", &windows)?;
    let parser = format!(
        "$errors = $null; [System.Management.Automation.Language.Parser]::ParseFile('{}', [ref]$null, [ref]$errors) > $null; if ($errors.Count) {{ throw $errors[0] }}",
        path.display()
    );
    success(
        Command::new("pwsh")
            .env("XDG_CACHE_HOME", path.with_extension("pwsh-cache"))
            .args(["-NoProfile", "-NonInteractive", "-Command", &parser]),
        "PowerShell 7.6.7 (pwsh)",
        "install PowerShell 7.6.7; see https://learn.microsoft.com/powershell/scripting/install/installing-powershell",
    )?;
    let guarded_side_effects = format!(
        "$errors = $null; $ast = [System.Management.Automation.Language.Parser]::ParseFile('{}', [ref]$null, [ref]$errors); if ($errors.Count) {{ exit 2 }}; $guard = $ast.FindAll({{ param($node) $node -is [System.Management.Automation.Language.TryStatementAst] }}, $true) | Where-Object {{ $_.Body.Extent.Text.Contains('starting runner bootstrap') }} | Select-Object -First 1; if ($null -eq $guard) {{ exit 3 }}; $body = $guard.Body.Extent.Text; $source = Get-Content -Raw '{}'; $prefix = $source.Substring(0, $guard.Extent.StartOffset); foreach ($effect in @('Add-Type -AssemblyName System.Security', 'Import-Certificate', '$sshKeys | Add-Content', 'Set-ItemProperty')) {{ if (-not $body.Contains($effect) -or $prefix.Contains($effect)) {{ exit 4 }} }}; if ($body.IndexOf('Import-Certificate') -gt $body.IndexOf(\"Send-Status -Status 'installing'\")) {{ exit 5 }}; if (-not $source.Contains('runner credential ACL setup failed') -or -not $source.Contains('SSH authorized-key ACL setup failed')) {{ exit 6 }}",
        path.display(),
        path.display()
    );
    success(
        Command::new("pwsh")
            .env("XDG_CACHE_HOME", path.with_extension("pwsh-cache"))
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &guarded_side_effects,
            ]),
        "PowerShell 7.6.7 (pwsh)",
        "install PowerShell 7.6.7; see https://learn.microsoft.com/powershell/scripting/install/installing-powershell",
    )?;
    assert_windows_ssh_assignments(&path, &ssh_keys)?;
    Ok(())
}

fn assert_windows_ssh_assignments(
    path: &Path,
    ssh_keys: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let expected_keys = ssh_keys
        .iter()
        .map(|key| format!("'{}'", key.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    let assignment_check = format!(
        "$tokens = $null; $errors = $null; $ast = [System.Management.Automation.Language.Parser]::ParseFile('{}', [ref]$tokens, [ref]$errors); $ast.FindAll({{ param($node) $node -is [System.Management.Automation.Language.AssignmentStatementAst] }}, $true) | Where-Object {{ $_.Left.Extent.Text -eq '$sshKeys' }} | ForEach-Object {{ Invoke-Expression $_.Extent.Text }}; $expected = @({expected_keys}); if ($sshKeys.Count -ne $expected.Count) {{ exit 3 }}; for ($i = 0; $i -lt $expected.Count; $i++) {{ if ($sshKeys[$i] -cne $expected[$i]) {{ exit 4 }} }}",
        path.display()
    );
    success(
        Command::new("pwsh")
            .env("XDG_CACHE_HOME", path.with_extension("pwsh-cache"))
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &assignment_check,
            ]),
        "PowerShell 7.6.7 (pwsh)",
        "install PowerShell 7.6.7; see https://learn.microsoft.com/powershell/scripting/install/installing-powershell",
    )?;
    Ok(())
}

#[test]
fn required_syntax_validators() {
    let directory = TestDirectory::new().unwrap();
    let linux = linux_instance().unwrap();
    let linux_render = common_render(
        &linux,
        BootstrapProvider::Aws,
        BootstrapPlatform::LinuxAmd64,
    )
    .unwrap();
    let shell = linux_script(&linux_render.cloud_config).unwrap();
    let linux_script_path = directory.write("linux.sh", &shell).unwrap();
    success(
        Command::new("bash").arg("-n").arg(&linux_script_path),
        "bash",
        "install Bash (present on Linux and macOS)",
    )
    .unwrap();
    let yaml_path = directory
        .write("cloud-config.yaml", &linux_render.cloud_config)
        .unwrap();
    success(
        Command::new("cloud-init")
            .args(["schema", "--config-file"])
            .arg(&yaml_path),
        "cloud-init 26.2",
        "install cloud-init 26.2; see https://cloudinit.readthedocs.io/en/26.2/reference/cli.html#schema",
    )
    .unwrap();

    validate_windows_bootstrap(&directory).unwrap();

    let mut mac_instance = linux_instance().unwrap();
    set_platform(&mut mac_instance, "macos", "arm64", "osx", "arm64");
    let mac =
        bootstrap::generate_macos(&mac_instance, target(BootstrapPlatform::MacosArm64)).unwrap();
    let mac_path = directory.write("macos.sh", mac.expose_secret()).unwrap();
    success(
        Command::new("bash").arg("-n").arg(&mac_path),
        "bash",
        "install Bash (present on Linux and macOS)",
    )
    .unwrap();
    let plist = mac
        .expose_secret()
        .split_once("<<'PLIST'\n")
        .unwrap()
        .1
        .split_once("\nPLIST")
        .unwrap()
        .0;
    let plist_path = directory.write("runner.plist", plist).unwrap();
    success(
        Command::new("plutil").arg("-lint").arg(&plist_path),
        "plutil",
        "install macOS Command Line Tools; plutil ships with macOS",
    )
    .unwrap();
}

#[test]
fn linux_agent_id_json_validation() {
    let script = delivered_script(
        &linux_instance().unwrap(),
        BootstrapProvider::Aws,
        BootstrapPlatform::LinuxAmd64,
    )
    .unwrap();
    let filter = script
        .split("jq -er '")
        .nth(1)
        .and_then(|rest| rest.split('\'').next())
        .unwrap();
    let directory = TestDirectory::new().unwrap();
    let path = directory.path.join("runner.json");
    for (input, valid) in [
        (r#"{"agent_id":7}"#, true),
        (r#"{"agentId":8}"#, true),
        (r#"{"agent_id":null,"agentId":8}"#, false),
        (r#"{"agent_id":false,"agentId":8}"#, false),
        (r#"{"agent_id":0}"#, false),
        (r#"{"agent_id":-1}"#, false),
        (r#"{"agent_id":2.5}"#, false),
        (r#"{"agent_id":"7"}"#, false),
        ("{}", false),
    ] {
        std::fs::write(&path, input).unwrap();
        let output = Command::new("jq")
            .args(["-er", filter])
            .arg(&path)
            .output()
            .unwrap();
        assert_eq!(output.status.success(), valid, "runner identity: {input}");
    }
}
