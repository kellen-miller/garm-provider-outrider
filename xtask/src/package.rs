//! Assemble a bounded, offline-verifiable Linux add-on from qualified binaries.
use crate::{bundle::sha256, root};
use anyhow::{Context, Result, ensure};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const HOST: &str = "linux/amd64";
const ASSETS: &[(&str, &str)] = &[
    ("LICENSE", "licenses/Outrider-LICENSE"),
    ("NOTICE", "licenses/NOTICE"),
    ("LICENSES/Apache-2.0.txt", "licenses/Apache-2.0.txt"),
    ("examples/garm.toml", "examples/garm.toml"),
    ("examples/config.toml", "examples/config.toml"),
    (
        "examples/config-minimal.toml",
        "examples/config-minimal.toml",
    ),
    ("examples/repository.toml", "examples/repository.toml"),
    ("packaging/outrider.service", "packaging/outrider.service"),
];

#[derive(Args)]
pub struct Options {
    #[arg(long)]
    binary: PathBuf,
    #[arg(long)]
    provider_bundle: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildInfo {
    version: String,
    source_revision: String,
    host: String,
    dirty: bool,
}

#[derive(Deserialize)]
struct SourceManifest {
    version: u64,
    interface_version: String,
    providers: Vec<SourceProvider>,
}

#[derive(Deserialize)]
struct SourceProvider {
    cloud: String,
    fork_repository: String,
    source_revision: String,
    expected_version: String,
    license: SourceLicense,
}

#[derive(Deserialize)]
struct SourceLicense {
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleManifest {
    manifest_version: u64,
    interface_version: String,
    host: String,
    go_version: String,
    qualification_scope: String,
    providers: Vec<BundleProvider>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleProvider {
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

#[derive(Deserialize, Serialize)]
struct PackageManifest {
    schema_version: u64,
    product: String,
    version: String,
    source_revision: String,
    host: String,
    files: Vec<FileRecord>,
    providers: Vec<ProviderRecord>,
}

#[derive(Deserialize, Serialize)]
struct FileRecord {
    path: String,
    sha256: String,
    mode: u32,
}

#[derive(Deserialize, Serialize)]
struct ProviderRecord {
    cloud: String,
    source_revision: String,
    expected_version: String,
    binary_path: String,
    binary_sha256: String,
    license_path: String,
    license_sha256: String,
}

/// Package only clean-source, native Linux amd64 static executables.
/// # Errors
/// Rejects stale build identity, unqualified providers, unsafe inputs and occupied output.
pub fn run(options: &Options) -> Result<()> {
    ensure!(
        std::env::consts::OS == "linux" && std::env::consts::ARCH == "x86_64",
        "packages must be assembled on native Linux amd64"
    );
    let revision = clean_revision(root())?;
    regular_file(&options.binary, true)?;
    let binary_digest = sha256(&options.binary)?;
    static_elf(&options.binary)?;
    let identity: BuildInfo = serde_json::from_str(&bounded_output(
        Command::new(&options.binary).arg("build-info"),
    )?)?;
    validate_identity(&identity, &revision, &options.version)?;
    ensure!(
        bounded_output(Command::new(&options.binary).arg("--version"))?.trim()
            == format!("outrider {}", options.version),
        "binary --version differs from build-info"
    );
    ensure!(
        bounded_output(
            Command::new(&options.binary)
                .env_clear()
                .env("GARM_COMMAND", "GetVersion")
                .env("GARM_CONTROLLER_ID", "package-smoke")
                .env(
                    "GARM_PROVIDER_CONFIG_FILE",
                    root().join("examples/config.toml")
                )
        )?
        .trim()
            == options.version,
        "binary GetVersion differs from build-info"
    );
    assemble(options, root(), &revision, &binary_digest)?;
    println!("package: {}", options.output.display());
    Ok(())
}

fn validate_identity(identity: &BuildInfo, revision: &str, version: &str) -> Result<()> {
    let semver = regex::Regex::new(
        r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*))?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$",
    )?;
    ensure!(semver.is_match(version), "package version must be semver");
    ensure!(
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "full source revision required"
    );
    ensure!(
        !identity.dirty
            && identity.source_revision == revision
            && identity.version == version
            && identity.host == HOST,
        "binary build-info must match clean checkout, version and host"
    );
    Ok(())
}

fn clean_revision(source: &Path) -> Result<String> {
    let status = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .current_dir(source)
        .output()?;
    ensure!(
        status.status.success() && status.stdout.is_empty(),
        "clean source checkout required"
    );
    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(source)
        .output()?;
    ensure!(revision.status.success(), "source checkout HEAD required");
    Ok(String::from_utf8(revision.stdout)?.trim().to_owned())
}

fn assemble(options: &Options, source: &Path, revision: &str, binary_digest: &str) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(&options.output) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "output must be a directory"
        );
        ensure!(
            fs::read_dir(&options.output)?.next().is_none(),
            "output must be empty"
        );
    }

    let mut pins: SourceManifest =
        serde_json::from_slice(&fs::read(source.join("providers/manifest.json"))?)?;
    let bundle_root = &options.provider_bundle;
    ensure!(
        fs::symlink_metadata(bundle_root)?.is_dir(),
        "bundle must be a real directory"
    );
    regular_file(&bundle_root.join("bundle-manifest.json"), false)?;
    let mut bundle: BundleManifest =
        serde_json::from_slice(&fs::read(bundle_root.join("bundle-manifest.json"))?)?;
    ensure!(
        pins.version == 1
            && bundle.manifest_version == pins.version
            && bundle.interface_version == pins.interface_version
            && bundle.host == HOST
            && bundle.go_version == "1.27.3"
            && bundle.qualification_scope == "source build and offline GetVersion only",
        "unsupported provider bundle metadata"
    );
    pins.providers
        .sort_by(|left, right| left.cloud.cmp(&right.cloud));
    bundle
        .providers
        .sort_by(|left, right| left.cloud.cmp(&right.cloud));
    let clouds: Vec<_> = bundle
        .providers
        .iter()
        .map(|provider| provider.cloud.as_str())
        .collect();
    ensure!(
        clouds == ["aws", "azure", "gcp"],
        "exactly aws/azure/gcp required"
    );
    ensure!(
        pins.providers
            .iter()
            .map(|provider| provider.cloud.as_str())
            .collect::<Vec<_>>()
            == clouds,
        "source provider set mismatch"
    );
    let mut inputs = BTreeMap::new();
    inputs.insert("bin/outrider".to_owned(), (options.binary.clone(), 0o755));
    let mut expected = BTreeSet::from([
        "bin".to_owned(),
        "licenses".to_owned(),
        "bundle-manifest.json".to_owned(),
    ]);
    let mut providers = Vec::new();
    for (pin, provider) in pins.providers.iter().zip(&bundle.providers) {
        let binary = format!("bin/garm-provider-{}", pin.cloud);
        let license = format!("licenses/{}-LICENSE", pin.cloud);
        ensure!(
            provider.binary == binary
                && provider.host == HOST
                && provider.repository == pin.fork_repository
                && provider.source_revision == pin.source_revision
                && provider.source_license_sha256 == pin.license.sha256
                && provider.get_version_stdout == pin.expected_version
                && provider.go_version == bundle.go_version,
            "{} bundle differs from source pins",
            pin.cloud
        );
        regular_file(&bundle_root.join(&binary), true)?;
        static_elf(&bundle_root.join(&binary))?;
        regular_file(&bundle_root.join(&license), false)?;
        ensure!(
            sha256(&bundle_root.join(&binary))? == provider.binary_sha256,
            "{} binary digest mismatch",
            pin.cloud
        );
        ensure!(
            sha256(&bundle_root.join(&license))? == pin.license.sha256,
            "{} license digest mismatch",
            pin.cloud
        );
        let binary_path = format!("providers/garm-provider-{}", pin.cloud);
        inputs.insert(binary_path.clone(), (bundle_root.join(&binary), 0o755));
        inputs.insert(license.clone(), (bundle_root.join(&license), 0o644));
        expected.insert(binary);
        expected.insert(license.clone());
        providers.push(ProviderRecord {
            cloud: pin.cloud.clone(),
            source_revision: pin.source_revision.clone(),
            expected_version: pin.expected_version.clone(),
            binary_path,
            binary_sha256: provider.binary_sha256.clone(),
            license_path: license,
            license_sha256: pin.license.sha256.clone(),
        });
    }

    ensure!(
        inventory(bundle_root)? == expected,
        "unexpected provider bundle inputs"
    );
    for (input, output) in ASSETS {
        inputs.insert((*output).into(), (source.join(input), 0o644));
    }

    let parent = options
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stage = tempfile::Builder::new()
        .prefix(".outrider-package-")
        .tempdir_in(parent)?;
    set_mode(stage.path(), 0o755)?;
    let mut files = Vec::new();
    for (relative, (input, mode)) in inputs {
        regular_file(&input, mode == 0o755)?;
        let destination = stage.path().join(&relative);
        fs::create_dir_all(destination.parent().context("package parent")?)?;
        for directory in destination
            .parent()
            .context("package parent")?
            .ancestors()
            .take_while(|directory| *directory != stage.path())
        {
            set_mode(directory, 0o755)?;
        }

        fs::copy(&input, &destination)?;
        set_mode(&destination, mode)?;
        files.push(FileRecord {
            path: relative,
            sha256: sha256(&destination)?,
            mode,
        });
    }

    ensure!(
        sha256(&stage.path().join("bin/outrider"))? == binary_digest,
        "Outrider changed while packaging"
    );
    for provider in &providers {
        ensure!(
            sha256(&stage.path().join(&provider.binary_path))? == provider.binary_sha256
                && sha256(&stage.path().join(&provider.license_path))? == provider.license_sha256,
            "provider changed while packaging"
        );
    }

    let manifest = PackageManifest {
        schema_version: 1,
        product: "outrider".into(),
        version: options.version.clone(),
        source_revision: revision.into(),
        host: HOST.into(),
        files,
        providers,
    };
    fs::write(
        stage.path().join("package-manifest.json"),
        format!("{}\n", serde_json::to_string_pretty(&manifest)?),
    )?;
    set_mode(&stage.path().join("package-manifest.json"), 0o644)?;
    ensure!(
        clean_revision(source)? == revision,
        "source changed while packaging"
    );
    // Rename can replace an empty directory, but never a populated directory.
    fs::rename(stage.path(), &options.output).context("publish package directory")?;
    Ok(())
}

fn regular_file(path: &Path, executable: bool) -> Result<()> {
    for ancestor in path.ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        ensure!(
            !fs::symlink_metadata(ancestor)?.file_type().is_symlink(),
            "symlink input: {}",
            ancestor.display()
        );
    }

    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file(),
        "regular input required: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            !executable || metadata.permissions().mode() & 0o111 != 0,
            "executable input required"
        );
    }
    #[cfg(not(unix))]
    ensure!(!executable, "Unix executable modes required");
    Ok(())
}

fn inventory(root: &Path) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(
                kind.is_file() || kind.is_dir(),
                "bundle contains non-regular input"
            );
            let path = entry.path();
            let relative = path.strip_prefix(root)?;
            ensure!(
                relative
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
                "unsafe bundle path"
            );
            paths.insert(relative.to_str().context("UTF-8 package path")?.to_owned());
            if kind.is_dir() {
                directories.push(path);
            }
        }
    }

    Ok(paths)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    anyhow::bail!("Unix file modes required: {} ({mode})", path.display());
    Ok(())
}

fn static_elf(path: &Path) -> Result<()> {
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let mut header = [0_u8; 64];
    file.read_exact(&mut header)?;
    ensure!(
        &header[..7] == b"\x7fELF\x02\x01\x01"
            && header[18..20] == [62, 0]
            && matches!(&header[16..18], [2, 0] | [3, 0]),
        "static Linux amd64 ELF required"
    );
    let offset = u64::from_le_bytes(header[32..40].try_into()?);
    let size = u16::from_le_bytes(header[54..56].try_into()?) as u64;
    let count = u16::from_le_bytes(header[56..58].try_into()?) as u64;
    ensure!(
        size >= 56
            && count > 0
            && offset >= 64
            && offset
                .checked_add(size * count)
                .is_some_and(|end| end <= length),
        "invalid ELF program headers"
    );
    let mut load = false;
    for index in 0..count {
        file.seek(SeekFrom::Start(offset + index * size))?;
        let mut program = [0_u8; 56];
        file.read_exact(&mut program)?;
        let kind = u32::from_le_bytes(program[..4].try_into()?);
        if kind == 2 {
            let offset = u64::from_le_bytes(program[8..16].try_into()?);
            let size = u64::from_le_bytes(program[32..40].try_into()?);
            ensure!(
                size % 16 == 0 && offset.checked_add(size).is_some_and(|end| end <= length),
                "invalid ELF dynamic segment"
            );
            file.seek(SeekFrom::Start(offset))?;
            for _ in 0..size / 16 {
                let mut entry = [0_u8; 16];
                file.read_exact(&mut entry)?;
                let tag = u64::from_le_bytes(entry[..8].try_into()?);
                ensure!(tag != 1, "ELF shared-library dependency prohibited");
                if tag == 0 {
                    break;
                }
            }
        }

        ensure!(
            kind != 3,
            "dynamic ELF interpreter prohibited; build x86_64-unknown-linux-musl"
        );
        load |= kind == 1;
    }

    ensure!(load, "ELF load segment required");
    Ok(())
}

fn bounded_output(command: &mut Command) -> Result<String> {
    let mut output = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output.try_clone()?);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        // SAFETY: pre_exec calls only async-signal-safe setrlimit syscalls.
        unsafe {
            command.pre_exec(|| {
                let output_limit = libc::rlimit {
                    rlim_cur: 65536,
                    rlim_max: 65536,
                };
                let core_limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &output_limit) == 0
                    && libc::setrlimit(libc::RLIMIT_CORE, &core_limit) == 0
                {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }
    let mut child = command.spawn()?;
    let start = Instant::now();
    let result = loop {
        if output.metadata()?.len() > 65536 || start.elapsed() > Duration::from_secs(5) {
            #[cfg(unix)]
            // SAFETY: child is placed in its own process group before spawn.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            child.kill().context("stop binary metadata process")?;
            child.wait().context("reap binary metadata process")?;
            anyhow::bail!("binary metadata exceeded time/output limit");
        }

        if let Some(status) = child.try_wait()? {
            break status;
        }

        std::thread::sleep(Duration::from_millis(10));
    };
    ensure!(result.success(), "binary metadata command failed");
    ensure!(
        output.metadata()?.len() <= 65536,
        "binary metadata output limit"
    );
    output.rewind()?;
    let mut text = String::new();
    output.take(65537).read_to_string(&mut text)?;
    Ok(text)
}

#[cfg(test)]
mod tests;
