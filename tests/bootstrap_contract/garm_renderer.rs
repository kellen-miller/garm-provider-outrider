use std::{
    env, fs, io,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use outrider::{
    bootstrap::{self, BootstrapPlatform, BootstrapProvider, BootstrapTarget},
    garm::BootstrapInstance,
};

use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value as YamlValue;

#[derive(Serialize)]
pub(super) struct CommonRenderRequest {
    pub(super) instance: serde_json::Value,
    pub(super) tools: serde_json::Value,
}

#[derive(Deserialize)]
pub(super) struct CommonRenderResult {
    pub(super) install_script: String,
    pub(super) cloud_config: String,
}

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

pub(super) struct TestDirectory {
    pub(super) path: PathBuf,
}

impl TestDirectory {
    pub(super) fn new() -> io::Result<Self> {
        let count = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outrider-bootstrap-{}-{nonce}-{count}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub(super) fn write(&self, name: &str, content: &str) -> io::Result<PathBuf> {
        let path = self.path.join(name);
        fs::write(&path, content)?;
        Ok(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            eprintln!("could not remove temporary test directory: {error}");
        }
    }
}

pub(super) fn linux_instance() -> Result<BootstrapInstance, serde_json::Error> {
    let mut instance: BootstrapInstance =
        serde_json::from_str(include_str!("../acceptance/fixtures/garm-bootstrap.json"))?;
    // The source fixture's CA marker is deliberately synthetic rather than PEM.
    // The pinned common renderer validates CA bundles before producing cloud-init.
    instance.ca_cert_bundle = None;
    Ok(instance)
}

pub(super) fn set_platform(
    instance: &mut BootstrapInstance,
    os: &str,
    arch: &str,
    tool_os: &str,
    tool_arch: &str,
) {
    instance.os_type = os.to_owned();
    instance.os_arch = arch.to_owned();
    instance.tools[0].os = Some(tool_os.to_owned());
    instance.tools[0].architecture = Some(tool_arch.to_owned());
    instance.tools[0].filename = Some("runner-test.archive".to_owned());
    instance.tools[0].sha256_checksum = Some("a".repeat(64));
}

pub(super) fn target(platform: BootstrapPlatform) -> BootstrapTarget {
    BootstrapTarget {
        platform,
        macos_user_data_enabled: true,
    }
}

pub(super) fn common_render(
    instance: &BootstrapInstance,
    provider: BootstrapProvider,
    platform: BootstrapPlatform,
) -> Result<CommonRenderResult, Box<dyn std::error::Error>> {
    let hook = bootstrap::generate_provider_hook(instance, target(platform), provider)?;
    let mut common_instance = serde_json::to_value(instance)?;
    common_instance["extra_specs"] = hook.extra_specs_json();
    common_instance["user_data_options"] = serde_json::to_value(&hook.user_data_options)?;
    let request = CommonRenderRequest {
        instance: common_instance,
        tools: serde_json::to_value(&instance.tools[0])?,
    };
    let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let harness = project.join("tests/fixtures/bootstrap-render");
    let cache = env::var_os("GOCACHE")
        .unwrap_or_else(|| project.join("target/bootstrap-go-cache").into_os_string());
    let mut child = Command::new("go")
        .args(["run", "-mod=readonly", "."])
        .current_dir(harness)
        .env("GOCACHE", cache)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let Some(mut stdin) = child.stdin.take() else {
        return Err(io::Error::other("actual common renderer has no stdin pipe").into());
    };
    serde_json::to_writer(&mut stdin, &request)?;
    drop(stdin);
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "actual common v0.1.9 renderer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }

    Ok(serde_json::from_slice(&output.stdout)?)
}

pub(super) fn delivered_script(
    instance: &BootstrapInstance,
    provider: BootstrapProvider,
    platform: BootstrapPlatform,
) -> Result<String, Box<dyn std::error::Error>> {
    let rendered = common_render(instance, provider, platform)?;
    match platform {
        BootstrapPlatform::LinuxAmd64 | BootstrapPlatform::LinuxArm64 => {
            linux_script(&rendered.cloud_config)
        }
        BootstrapPlatform::WindowsAmd64 => Ok(rendered.install_script),
        BootstrapPlatform::MacosArm64 => {
            Err(io::Error::other("macOS does not use the pinned common provider renderer").into())
        }
    }
}

pub(super) fn hook_script(specs: &serde_json::Value) -> Result<String, Box<dyn std::error::Error>> {
    let encoded = specs["runner_install_template"]
        .as_str()
        .ok_or_else(|| io::Error::other("hook extra specs lack a byte template"))?;
    let template = String::from_utf8(STANDARD.decode(encoded)?)?;
    let literal = template
        .strip_prefix("{{")
        .and_then(|value| value.strip_suffix("}}"))
        .ok_or_else(|| io::Error::other("hook template is not a single Go literal"))?;
    Ok(serde_json::from_str(literal)?)
}

pub(super) fn linux_script(content: &str) -> Result<String, Box<dyn std::error::Error>> {
    let yaml = content
        .strip_prefix("#cloud-config\n")
        .ok_or_else(|| io::Error::other("missing cloud-config header"))?;
    let config: YamlValue = serde_yaml_ng::from_str(yaml)?;
    let files = config["write_files"]
        .as_sequence()
        .ok_or_else(|| io::Error::other("cloud-config write_files is missing"))?;
    let file = files
        .iter()
        .find(|file| file["path"].as_str() == Some("/install_runner.sh"))
        .ok_or_else(|| io::Error::other("cloud-config runner install script is missing"))?;
    let encoded = file["content"]
        .as_str()
        .ok_or_else(|| io::Error::other("cloud-config bootstrap script has no content"))?;
    Ok(String::from_utf8(STANDARD.decode(encoded)?)?)
}

pub(super) fn success(
    command: &mut Command,
    expected_tool: &str,
    setup: &str,
) -> io::Result<Output> {
    let output = command.output().map_err(|error| {
        io::Error::other(format!(
            "{expected_tool} is required for bootstrap syntax validation; {setup}; {error}"
        ))
    })?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "{expected_tool} rejected guest script: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(output)
}
