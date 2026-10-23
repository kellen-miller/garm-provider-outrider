//! Build and qualify immutable provider sources without cloud provisioning.
use crate::{process, root};
use anyhow::{Context, Result, ensure};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Args)]
pub struct Options {
    #[arg(long)]
    go_version: String,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    source_cache: Option<PathBuf>,
    #[arg(long)]
    go_cache_root: Option<PathBuf>,
    #[arg(long)]
    plan: bool,
}

#[derive(Deserialize)]
struct Manifest {
    version: u64,
    interface_version: String,
    providers: Vec<Provider>,
}

#[derive(Deserialize)]
struct Provider {
    cloud: String,
    fork_repository: String,
    source_revision: String,
    expected_version: String,
    license: License,
}

#[derive(Deserialize)]
struct License {
    path: PathBuf,
    sha256: String,
}

#[derive(Serialize)]
struct Record {
    cloud: String,
    host: String,
    repository: String,
    source_revision: String,
    source_license_sha256: String,
    go_version: String,
    binary: String,
    binary_sha256: String,
    get_version_stdout: String,
}

/// Hash source licenses and host binaries.
/// # Errors
/// Returns file read errors.
pub fn sha256(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }

        digest.update(&buffer[..count]);
    }

    Ok(hex::encode(digest.finalize()))
}

/// Build exact provider pins and verify their offline GetVersion interface.
/// # Errors
/// Rejects host, version, source, origin, license, or binary qualification failures.
pub fn build(options: &Options) -> Result<()> {
    let go = std::env::var_os("GO").unwrap_or_else(|| "go".into());
    let version = process::run(Command::new(&go).arg("version"))?;
    ensure!(
        version.starts_with(&format!("go version go{} ", options.go_version)),
        "unexpected Go version: {}",
        version.trim()
    );
    let goos = process::run(Command::new(&go).args(["env", "GOOS"]))?;
    let goarch = process::run(Command::new(&go).args(["env", "GOARCH"]))?;
    let host = format!("{}/{}", goos.trim(), goarch.trim());
    let os = if std::env::consts::OS == "macos" {
        "darwin"
    } else {
        std::env::consts::OS
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => anyhow::bail!("unsupported bundle architecture"),
    };
    ensure!(
        host == format!("{os}/{arch}")
            && matches!(
                host.as_str(),
                "linux/amd64" | "linux/arm64" | "darwin/arm64" | "windows/amd64"
            ),
        "unsupported or mismatched bundle host: {host}"
    );
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(root().join("providers/manifest.json"))?)?;
    println!("bundle host: {host}");
    if options.plan {
        for provider in manifest.providers {
            println!(
                "{}: {}@{}",
                provider.cloud, provider.fork_repository, provider.source_revision
            );
        }

        return Ok(());
    }

    fs::create_dir_all(&options.output)?;
    let output = options.output.canonicalize()?;
    ensure!(
        fs::read_dir(&output)?.next().is_none(),
        "output directory must be empty"
    );
    let temporary = tempfile::tempdir()?;
    let source_root = options
        .source_cache
        .clone()
        .unwrap_or_else(|| temporary.path().join("sources"));
    let cache = options
        .go_cache_root
        .clone()
        .unwrap_or_else(|| temporary.path().join("go-cache"));
    fs::create_dir_all(&source_root)?;
    fs::create_dir_all(&cache)?;
    let source_root = source_root.canonicalize()?;
    let cache = cache.canonicalize()?;
    fs::create_dir(output.join("bin"))?;
    fs::create_dir(output.join("licenses"))?;
    let mut records = Vec::new();
    for provider in manifest.providers {
        let source = source_checkout(&provider, &source_root)?;
        let relative_binary = format!(
            "bin/garm-provider-{}{}",
            provider.cloud,
            std::env::consts::EXE_SUFFIX
        );
        let binary = output.join(&relative_binary);
        let provider_cache = if options.go_cache_root.is_some() {
            cache.join(format!("{}-cache", provider.cloud))
        } else {
            cache.clone()
        };
        let symbol = format!(
            "-X=github.com/cloudbase/garm-provider-{}/provider.Version={}",
            provider.cloud, provider.expected_version
        );
        process::run(
            Command::new(&go)
                .args([
                    "build",
                    "-mod=vendor",
                    "-trimpath",
                    "-buildvcs=false",
                    "-ldflags",
                    &symbol,
                    "-o",
                ])
                .arg(&binary)
                .arg(".")
                .current_dir(&source)
                .env("GOOS", goos.trim())
                .env("GOARCH", goarch.trim())
                .env("CGO_ENABLED", "0")
                .env("GOMAXPROCS", "2")
                .env("GOFLAGS", "-p=2")
                .env("GOCACHE", provider_cache),
        )?;
        let config = version_configuration(&provider.cloud, temporary.path())?;
        let result = process::run(
            Command::new(&binary)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("GARM_COMMAND", "GetVersion")
                .env("GARM_CONTROLLER_ID", "qualification-only")
                .env("GARM_PROVIDER_CONFIG_FILE", config)
                .env("AWS_EC2_METADATA_DISABLED", "true"),
        )?;
        ensure!(
            result.trim() == provider.expected_version,
            "{} returned unexpected GetVersion",
            provider.cloud
        );
        let license_path = source.join(&provider.license.path);
        fs::copy(
            &license_path,
            output
                .join("licenses")
                .join(format!("{}-LICENSE", provider.cloud)),
        )?;
        records.push(Record {
            cloud: provider.cloud,
            host: host.clone(),
            repository: provider.fork_repository,
            source_revision: provider.source_revision,
            source_license_sha256: sha256(&license_path)?,
            go_version: options.go_version.clone(),
            binary: relative_binary,
            binary_sha256: sha256(&binary)?,
            get_version_stdout: result.trim().to_owned(),
        });
    }

    let bundle = json!({"manifest_version":manifest.version,"interface_version":manifest.interface_version,"host":host,"go_version":options.go_version,"qualification_scope":"source build and offline GetVersion only","providers":records});
    fs::write(
        output.join("bundle-manifest.json"),
        format!("{}\n", serde_json::to_string_pretty(&bundle)?),
    )?;
    println!(
        "bundle manifest: {}",
        output.join("bundle-manifest.json").display()
    );
    Ok(())
}

fn source_checkout(provider: &Provider, root: &Path) -> Result<PathBuf> {
    ensure!(
        ["aws", "azure", "gcp"].contains(&provider.cloud.as_str()),
        "unknown provider cloud"
    );
    ensure!(
        provider.source_revision.len() == 40
            && provider
                .source_revision
                .bytes()
                .all(|b| b.is_ascii_hexdigit()),
        "full source revision required"
    );
    let mut source = root.join(&provider.cloud);
    let existing = root.join(format!("garm-provider-{}", provider.cloud));
    if !source.is_dir() && existing.is_dir() {
        source = existing;
    }

    let fresh = !source.exists();
    if fresh {
        process::run(
            Command::new("git")
                .args([
                    "clone",
                    "--no-checkout",
                    &format!("https://github.com/{}.git", provider.fork_repository),
                ])
                .arg(&source),
        )?;
    }

    let origin = process::run(
        Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["remote", "get-url", "origin"]),
    )?;
    let normalized = origin
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_start_matches("https://")
        .trim_start_matches("ssh://git@")
        .trim_start_matches("git@")
        .replace("github.com:", "github.com/");
    ensure!(
        normalized == format!("github.com/{}", provider.fork_repository),
        "unexpected source origin"
    );
    if fresh {
        process::run(Command::new("git").arg("-C").arg(&source).args([
            "fetch",
            "--depth",
            "1",
            "origin",
            &provider.source_revision,
        ]))?;
        process::run(Command::new("git").arg("-C").arg(&source).args([
            "checkout",
            "--detach",
            &provider.source_revision,
        ]))?;
    }

    let actual = process::run(
        Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["rev-parse", "HEAD"]),
    )?;
    ensure!(
        actual.trim() == provider.source_revision,
        "source cache revision mismatch"
    );
    let status = process::run(Command::new("git").arg("-C").arg(&source).args([
        "status",
        "--porcelain",
        "--untracked-files=all",
    ]))?;
    ensure!(status.is_empty(), "source checkout is dirty");
    let license = source
        .join(&provider.license.path)
        .canonicalize()
        .context("source license")?;
    ensure!(
        license.starts_with(source.canonicalize()?),
        "license escapes source checkout"
    );
    ensure!(
        sha256(&license)? == provider.license.sha256,
        "source license digest mismatch"
    );
    Ok(source)
}

fn version_configuration(cloud: &str, directory: &Path) -> Result<PathBuf> {
    let path = directory.join(format!("{cloud}.toml"));
    let config = match cloud {
        "aws" => "region = \"us-east-1\"\nsubnet_id = \"subnet-qualification-only\"\n[credentials]\ncredential_type = \"static\"\n[credentials.static]\naccess_key_id = \"QUALIFICATION_ONLY\"\nsecret_access_key = \"qualification-only-secret\"\n".to_owned(),
        "azure" => "location = \"eastus\"\n[credentials]\nsubscription_id = \"00000000-0000-0000-0000-000000000001\"\n[credentials.service_principal]\ntenant_id = \"00000000-0000-0000-0000-000000000002\"\nclient_id = \"00000000-0000-0000-0000-000000000003\"\nclient_secret = \"qualification-only-secret\"\n".to_owned(),
        _ => {
            let key = directory.join("qualification-only-key.pem");
            process::run(Command::new("openssl").args(["genrsa", "-out"]).arg(&key).arg("2048"))?;
            let credentials = directory.join("qualification-only-service-account.json");
            fs::write(&credentials, serde_json::to_vec(&json!({"type":"service_account","project_id":"qualification-only-project","private_key_id":"qualification-only","private_key":fs::read_to_string(key)?,"client_email":"qualification-only@qualification-only-project.iam.gserviceaccount.com","client_id":"000000000000000000000","token_uri":"https://oauth2.googleapis.com/token"}))?)?;
            format!("project_id = \"qualification-only-project\"\nzone = \"us-central1-a\"\nnetwork_id = \"qualification-only-network\"\nsubnetwork_id = \"projects/qualification-only-project/regions/us-central1/subnetworks/qualification-only\"\ncredentials_file = {}\n", serde_json::to_string(&credentials)?)
        }
    };
    fs::write(&path, config)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SourceFixture {
        directory: tempfile::TempDir,
        provider: Provider,
    }

    impl SourceFixture {
        fn new() -> Result<Self> {
            let directory = tempfile::tempdir()?;
            let source = directory.path().join("aws");
            fs::create_dir(&source)?;
            fs::write(source.join("LICENSE"), "fixture license\n")?;
            process::run(
                Command::new("git")
                    .args(["init", "--quiet", "--object-format=sha1"])
                    .arg(&source),
            )?;
            process::run(Command::new("git").arg("-C").arg(&source).args([
                "remote",
                "add",
                "origin",
                "https://github.com/kellen-miller/garm-provider-aws.git",
            ]))?;
            process::run(
                Command::new("git")
                    .arg("-C")
                    .arg(&source)
                    .args(["add", "LICENSE"]),
            )?;
            process::run(Command::new("git").arg("-C").arg(&source).args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--quiet",
                "-m",
                "test: pin fixture",
            ]))?;
            let revision = process::run(
                Command::new("git")
                    .arg("-C")
                    .arg(&source)
                    .args(["rev-parse", "HEAD"]),
            )?;
            let provider = Provider {
                cloud: "aws".to_owned(),
                fork_repository: "kellen-miller/garm-provider-aws".to_owned(),
                source_revision: revision.trim().to_owned(),
                expected_version: "fixture".to_owned(),
                license: License {
                    path: "LICENSE".into(),
                    sha256: sha256(&source.join("LICENSE"))?,
                },
            };
            Ok(Self {
                directory,
                provider,
            })
        }
    }

    #[test]
    fn exact_clean_source_is_reusable_without_checkout_mutations() -> Result<()> {
        let fixture = SourceFixture::new()?;
        let first = source_checkout(&fixture.provider, fixture.directory.path())?;
        assert_eq!(
            source_checkout(&fixture.provider, fixture.directory.path())?,
            first
        );
        Ok(())
    }

    #[test]
    fn source_revision_license_and_dirty_checkout_fail_closed() -> Result<()> {
        let mut fixture = SourceFixture::new()?;
        let revision = fixture.provider.source_revision.clone();
        fixture.provider.source_revision = "0".repeat(40);
        assert!(source_checkout(&fixture.provider, fixture.directory.path()).is_err());
        fixture.provider.source_revision = revision;
        let digest = fixture.provider.license.sha256.clone();
        fixture.provider.license.sha256 = "0".repeat(64);
        assert!(source_checkout(&fixture.provider, fixture.directory.path()).is_err());
        fixture.provider.license.sha256 = digest;
        fs::write(fixture.directory.path().join("aws/untracked"), "dirty")?;
        assert!(source_checkout(&fixture.provider, fixture.directory.path()).is_err());
        Ok(())
    }

    #[test]
    fn wrong_origin_cannot_reuse_a_cached_checkout() -> Result<()> {
        let fixture = SourceFixture::new()?;
        process::run(
            Command::new("git")
                .arg("-C")
                .arg(fixture.directory.path().join("aws"))
                .args([
                    "remote",
                    "set-url",
                    "origin",
                    "https://github.com/other/provider.git",
                ]),
        )?;
        assert!(source_checkout(&fixture.provider, fixture.directory.path()).is_err());
        Ok(())
    }
}
