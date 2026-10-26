//! Exercise an actual package, including its provider executable inside upstream GARM.

use crate::{process, root};
use anyhow::{Context, Result, ensure};
use clap::Args;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};
use toml_edit::{DocumentMut, value};

#[derive(Args)]
pub struct Options {
    #[arg(long)]
    package: PathBuf,
    #[arg(
        long,
        default_value = "ghcr.io/cloudbase/garm@sha256:c74079c24bdf9dacbd55f9ac29f149502b909827388430205b8cca65b04a7dda"
    )]
    garm_image: String,
    /// Also probe the packaged OCI runtime without publishing it.
    #[arg(long)]
    image: Option<String>,
}

/// Use disposable owned files, synthetic authority and no cloud API requests.
/// # Errors
/// Rejects failed install/setup/doctor, preservation, OCI or authenticated protocol checks.
pub fn check(options: &Options) -> Result<()> {
    ensure!(
        std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64",
        "native Linux amd64 required"
    );
    let package = options.package.canonicalize()?;
    let binary = package.join("bin/outrider");
    let directory = tempfile::Builder::new()
        .prefix("install-smoke-")
        .tempdir_in(root().join("target"))?;
    let canonical = directory.path().canonicalize()?;
    let base = canonical.as_path();
    let prefix = base.join("installed");
    let config = base.join("config/config.toml");
    let state = base.join("state");
    let unit = base.join("outrider.service");
    // SAFETY: process uid/gid queries have no memory-safety preconditions.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let install = || {
        process::run(
            Command::new(&binary)
                .arg("install")
                .arg("--package")
                .arg(&package)
                .arg("--prefix")
                .arg(&prefix)
                .arg("--config")
                .arg(&config)
                .arg("--state")
                .arg(&state)
                .arg("--systemd-unit")
                .arg(&unit)
                .arg("--uid")
                .arg(uid.to_string())
                .arg("--gid")
                .arg(gid.to_string()),
        )
    };
    install()?;
    let original_secret = fs::read(state.join("control.token"))?;
    install()?;
    ensure!(
        fs::read(state.join("control.token"))? == original_secret,
        "repeat install replaced control secret"
    );
    let mut authored = fs::read_to_string(&config)?.parse::<DocumentMut>()?;
    let mut native = DocumentMut::new();
    native["apiserver"]["bind"] = value("127.0.0.1");
    native["apiserver"]["port"] = value(9980);
    native["apiserver"]["use_tls"] = value(false);
    let mut providers = toml_edit::ArrayOfTables::new();
    for cloud in ["aws", "azure", "gcp"] {
        let child = base.join(format!("{cloud}.toml"));
        let content = match cloud {
            "aws" => "region='us-east-1'\nsubnet_id='subnet-fixture'\n[credentials]\ncredential_type='static'\n[credentials.static]\naccess_key_id='fixture'\nsecret_access_key='fixture'\n".to_owned(),
            "azure" => "location='eastus'\n[credentials]\nsubscription_id='azure-synthetic'\n[credentials.service_principal]\ntenant_id='fixture'\nclient_id='fixture'\nclient_secret='fixture'\n".to_owned(),
            "gcp" => format!("project_id='fixture-project'\nzone='us-central1-a'\ncredentials_file={}\nnetwork_id='global/networks/fixture'\nsubnetwork_id='regions/us-central1/subnetworks/fixture'\n", serde_json::to_string(&base.join("gcp-key.json"))?),
            _ => unreachable!(),
        };
        fs::write(&child, content)?;
        let mut provider = toml_edit::Table::new();
        provider["name"] = value(format!("{cloud}-main"));
        provider["provider_type"] = value("external");
        provider["external"]["interface_version"] = value("v0.1.0");
        provider["external"]["provider_executable"] = value(
            prefix
                .join(format!("providers/garm-provider-{cloud}"))
                .display()
                .to_string(),
        );
        provider["external"]["config_file"] = value(child.display().to_string());
        providers.push(provider);
    }

    native["provider"] = toml_edit::Item::ArrayOfTables(providers);
    let garm = base.join("garm.toml");
    fs::write(&garm, native.to_string())?;
    protected_mode(&garm, 0o600)?;
    authored["garm"]["config_file"] = value(garm.display().to_string());
    fs::write(&config, authored.to_string())?;
    let setup = || {
        process::run(
            Command::new(&binary)
                .arg("setup")
                .arg("--config")
                .arg(&config)
                .arg("--package")
                .arg(&prefix)
                .arg("--register"),
        )
    };
    setup()?;
    let configured = fs::read(&config)?;
    let registered = fs::read(&garm)?;
    setup()?;
    ensure!(
        fs::read(&config)? == configured && fs::read(&garm)? == registered,
        "setup rerun changed qualified state"
    );
    let parsed: Value = toml_edit::de::from_slice(&registered)?;
    ensure!(
        parsed["provider"]
            .as_array()
            .context("missing registry")?
            .len()
            == 4,
        "delegate registry changed"
    );
    let report = process::run(
        Command::new(&binary)
            .arg("doctor")
            .arg("--config")
            .arg(&config),
    )?;
    let report: Value = serde_json::from_str(&report)?;
    ensure!(
        report["configuration_valid"] == true && report["runner_execution_verified"] == false,
        "doctor overstated static qualification"
    );
    let preserved = fs::read(&config)?;
    install()?;
    ensure!(
        fs::read(&config)? == preserved,
        "install overwrote authored configuration"
    );
    container_provider_roundtrip(&package, &options.garm_image, base, false)?;
    if let Some(image) = &options.image {
        let output = process::run(Command::new("docker").args([
            "run",
            "--rm",
            "--platform",
            "linux/amd64",
            "--network",
            "none",
            "--read-only",
            image,
            "build-info",
        ]))?;
        let built: Value = serde_json::from_str(&output)?;
        ensure!(
            built["host"] == "linux/amd64" && built["dirty"] == false,
            "invalid OCI identity"
        );
        container_installation(image, base, &package, uid, gid)?;
        container_provider_roundtrip(&package, image, base, true)?;
    }

    println!(
        "package install/setup/static-doctor and authenticated GARM-container roundtrip passed; no cloud runner allocated"
    );
    Ok(())
}

fn container_installation(
    image: &str,
    base: &Path,
    package: &Path,
    uid: u32,
    gid: u32,
) -> Result<()> {
    let directory = base.join("image-install");
    fs::create_dir(&directory)?;
    let install = || {
        process::run(
            Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    "--platform",
                    "linux/amd64",
                    "--network",
                    "none",
                    "--read-only",
                    "--user",
                ])
                .arg(format!("{uid}:{gid}"))
                .arg("--volume")
                .arg(format!("{}:/smoke", directory.display()))
                .args([
                    image,
                    "install",
                    "--package",
                    "/usr/share/outrider/package",
                    "--prefix",
                    "/smoke/installed",
                    "--config",
                    "/smoke/config/config.toml",
                    "--state",
                    "/smoke/state",
                    "--uid",
                ])
                .arg(uid.to_string())
                .arg("--gid")
                .arg(gid.to_string()),
        )
    };
    install()?;
    let secret = fs::read(directory.join("state/control.token"))?;
    install()?;
    ensure!(
        fs::read(directory.join("state/control.token"))? == secret,
        "container repeat install changed authority"
    );
    for relative in [
        "bin/outrider",
        "providers/garm-provider-aws",
        "providers/garm-provider-azure",
        "providers/garm-provider-gcp",
        "package-manifest.json",
    ] {
        ensure!(
            fs::read(directory.join("installed").join(relative))?
                == fs::read(package.join(relative))?,
            "OCI package differs from qualified archive: {relative}"
        );
    }

    Ok(())
}

fn container_provider_roundtrip(
    package: &Path,
    image: &str,
    base: &Path,
    product_image: bool,
) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let runtime = base.join(if product_image {
        "image-protocol"
    } else {
        "protocol"
    });
    fs::create_dir(&runtime)?;
    fs::write(runtime.join("token"), "protocol-fixture-authority")?;
    fs::write(
        runtime.join("config.toml"),
        format!(
            "version=1\n[control]\nlisten_address='{address}'\ndatabase_path='/unused'\nauth_token_file='/smoke/token'\nprovider_timeout_seconds=10\n"
        ),
    )?;
    let worker = thread::spawn(move || -> Result<()> {
        listener.set_nonblocking(true)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(20))
                }
                Err(error) => return Err(error.into()),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        let request = loop {
            let count = stream.read(&mut buffer)?;
            ensure!(
                count > 0 && bytes.len() + count <= 2 * 1024 * 1024,
                "invalid protocol request"
            );
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&bytes[..offset])?;
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(str::trim)
                            .map(str::to_owned)
                    })
                    .context("missing request length")?
                    .parse::<usize>()?;
                if bytes.len() < offset + 4 + length {
                    continue;
                }
                ensure!(
                    headers.starts_with("POST /v1/provider ")
                        && headers
                            .to_ascii_lowercase()
                            .contains("authorization: bearer protocol-fixture-authority"),
                    "protocol authority mismatch"
                );
                break serde_json::from_slice::<Value>(&bytes[offset + 4..offset + 4 + length])?;
            }
        };
        ensure!(
            request["command"] == "GetInstance" && request["instance_id"] == "instance-fixture",
            "incorrect lifecycle request"
        );
        let response = json!({"result":{"provider_id":"instance-fixture","name":"runner-fixture","status":"running"},"error":null,"not_found":false}).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )?;
        stream.flush()?;
        Ok(())
    });
    let output = process::run(
        Command::new("docker")
            .args([
                "run",
                "--rm",
                "--platform",
                "linux/amd64",
                "--network",
                "host",
                "--read-only",
                "--entrypoint",
                if product_image {
                    "/usr/share/outrider/package/bin/outrider"
                } else {
                    "/opt/outrider/bin/outrider"
                },
            ])
            .arg("--volume")
            .arg(format!("{}:/opt/outrider:ro", package.display()))
            .arg("--volume")
            .arg(format!("{}:/smoke:ro", runtime.display()))
            .args([
                "--env",
                "GARM_COMMAND=GetInstance",
                "--env",
                "GARM_CONTROLLER_ID=11111111-1111-4111-8111-111111111111",
                "--env",
                "GARM_INSTANCE_ID=instance-fixture",
                "--env",
                "GARM_PROVIDER_CONFIG_FILE=/smoke/config.toml",
                "--env",
                "HTTP_PROXY=http://127.0.0.1:1",
                "--env",
                "ALL_PROXY=http://127.0.0.1:1",
                "--env",
                "NO_PROXY=",
                image,
            ]),
    );
    let server = worker
        .join()
        .map_err(|_| anyhow::anyhow!("protocol fixture panicked"))?;
    server?;
    let result: Value = serde_json::from_str(&output?)?;
    ensure!(
        result["provider_id"] == "instance-fixture",
        "container lifecycle result mismatch"
    );
    Ok(())
}

fn protected_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
