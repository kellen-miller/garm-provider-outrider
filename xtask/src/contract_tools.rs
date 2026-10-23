//! Pinned native bootstrap validators and the tagged Go wrapper probe.
use crate::{bundle::sha256, process, root};
use anyhow::{Result, ensure};
use std::{fs, path::Path, process::Command};

/// Install pinned external validators; Python is exclusively cloud-init's runtime.
/// # Errors
/// Rejects unsupported hosts, versions, checksums, or failed tool invocations.
pub fn install(output: &Path) -> Result<()> {
    ensure!(
        std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64",
        "native validator installation requires macOS arm64"
    );
    ensure!(
        process::run(Command::new("python3").arg("--version"))?.trim() == "Python 3.15.0",
        "cloud-init requires Python 3.15.0"
    );
    fs::create_dir_all(output)?;
    let output = output.canonicalize()?;
    fs::create_dir_all(output.join("src"))?;
    fs::create_dir_all(output.join("bin"))?;
    let temporary = tempfile::tempdir()?;
    for (url, digest, archive, destination) in [
        (
            "https://codeload.github.com/canonical/cloud-init/tar.gz/refs/tags/26.2",
            "16274a4102426871cad84ad07db41f82aa8f4d43d95e5947a62e199c2df775c6",
            "cloud-init.tar.gz",
            output.join("src"),
        ),
        (
            "https://github.com/PowerShell/PowerShell/releases/download/v7.6.7/powershell-7.6.7-osx-arm64.tar.gz",
            "50dde99bb066adacbb9a748182d09eb9beff25119dc3b12e85c84b6b6b1c5da9",
            "powershell.tar.gz",
            output.join("powershell"),
        ),
    ] {
        let archive = temporary.path().join(archive);
        process::run(
            Command::new("curl")
                .args([
                    "--fail",
                    "--location",
                    "--silent",
                    "--show-error",
                    url,
                    "--output",
                ])
                .arg(&archive),
        )?;
        ensure!(sha256(&archive)? == digest, "validator checksum mismatch");
        fs::create_dir_all(&destination)?;
        process::run(
            Command::new("tar")
                .arg("-xzf")
                .arg(&archive)
                .arg("-C")
                .arg(&destination),
        )?;
    }

    let source = output.join("src/cloud-init-26.2");
    let venv = output.join("cloud-init-venv");
    process::run(Command::new("python3").args(["-m", "venv"]).arg(&venv))?;
    let python = venv.join("bin/python");
    process::run(
        Command::new(&python)
            .args(["-m", "pip", "install", "--disable-pip-version-check", "-r"])
            .arg(source.join("requirements.txt")),
    )?;
    let wrapper = output.join("bin/cloud-init");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexport PYTHONPATH={}\nexec {} {} \"$@\"\n",
            shell_quote(&source),
            shell_quote(&python),
            shell_quote(&source.join("scripts/cloud-init"))
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755))?;
        fs::set_permissions(
            output.join("powershell/pwsh"),
            fs::Permissions::from_mode(0o755),
        )?;
        let link = output.join("bin/pwsh");
        if link.exists() {
            fs::remove_file(&link)?;
        }

        symlink(output.join("powershell/pwsh"), &link)?;
    }

    ensure!(
        process::run(Command::new(&wrapper).arg("--version"))?.contains("26.2"),
        "unexpected cloud-init version"
    );
    ensure!(
        process::run(
            Command::new(output.join("bin/pwsh"))
                .env("XDG_CACHE_HOME", output.join("powershell-cache"))
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-Command",
                    "$PSVersionTable.PSVersion.ToString()"
                ])
        )?
        .trim()
            == "7.6.7",
        "unexpected PowerShell version"
    );
    process::run(Command::new("plutil").arg("-help"))?;
    Ok(())
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// Build the real pinned GARM external-provider wrapper with supported Go.
/// # Errors
/// Returns unsupported Go versions or failed compilation.
pub fn build_wrapper(output: &Path) -> Result<()> {
    let version = process::run(Command::new("go").arg("version"))?;
    let parsed = version
        .split_whitespace()
        .nth(2)
        .and_then(|v| v.strip_prefix("go"))
        .ok_or_else(|| anyhow::anyhow!("invalid Go version"))?;
    let numbers: Vec<u64> = parsed
        .split('.')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    ensure!(
        numbers.as_slice() >= [1, 26, 2].as_slice(),
        "GARM wrapper requires Go 1.26.2 or newer"
    );
    let output = if output.is_absolute() {
        output.to_path_buf()
    } else {
        std::env::current_dir()?.join(output)
    };
    fs::create_dir_all(
        output
            .parent()
            .ok_or_else(|| anyhow::anyhow!("wrapper output requires parent"))?,
    )?;
    process::run(
        Command::new("go")
            .args(["build", "-p", "2", "-buildvcs=false", "-o"])
            .arg(&output)
            .arg(".")
            .current_dir(root().join("tests/fixtures/system/garm-wrapper-probe"))
            .env("GOMAXPROCS", "2"),
    )?;
    Ok(())
}
