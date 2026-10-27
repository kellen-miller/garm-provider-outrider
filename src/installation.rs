//! Offline package verification and immutable, descriptor-relative installation.
//!
//! A manifest establishes integrity only. Download checksums and trusted build
//! provenance must establish the package's source before invoking this module.

use clap::Args;
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[path = "installation/filesystem.rs"]
mod filesystem;
#[path = "installation/package.rs"]
mod package;

use filesystem::{InstallationTransaction, check_destination, existing_file, open_directory};
use package::VerifiedPackage;
pub use package::{PackageFile, PackageManifest, PackageProvider};

const MANIFEST_LIMIT: u64 = 64 * 1024;
const FILE_LIMIT: u64 = 512 * 1024 * 1024;
const TOTAL_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
const MANIFEST_NAME: &str = "package-manifest.json";

#[derive(Debug, Args)]
pub struct InstallOptions {
    #[arg(long)]
    pub package: PathBuf,
    #[arg(long, default_value = "/opt/outrider")]
    pub prefix: PathBuf,
    #[arg(long, default_value = "/etc/outrider/config.toml")]
    pub config: PathBuf,
    #[arg(long, default_value = "/var/lib/outrider")]
    pub state: PathBuf,
    #[arg(long)]
    pub systemd_unit: Option<PathBuf>,
    #[arg(long)]
    pub uid: u32,
    #[arg(long)]
    pub gid: u32,
}

#[derive(Debug, Serialize)]
pub struct InstallReport {
    pub prefix: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub control_token: PathBuf,
    pub systemd_unit: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallationError {
    #[error("{0}")]
    Invalid(String),
    #[error("installation filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("invalid package manifest: {0}")]
    Manifest(#[from] serde_json::Error),
}

struct PlannedFile {
    path: PathBuf,
    bytes: Vec<u8>,
    mode: u32,
    uid: u32,
    gid: u32,
}

/// Validate every package member, including ELF linkage, before exposing pins.
///
/// # Errors
/// Rejects unsafe paths, incomplete manifests, changed artifacts or unsupported binaries.
pub fn inspect_package(package: &Path) -> Result<PackageManifest, InstallationError> {
    Ok(VerifiedPackage::read(package)?.manifest)
}

/// Install a fully verified package. Existing prefixes are immutable; config and
/// secrets are preserved. On failure only entries created by this call roll back.
///
/// # Errors
/// Rejects unsafe destinations, changed existing files, invalid identity or failed writes.
pub fn install(options: &InstallOptions) -> Result<InstallReport, InstallationError> {
    // SAFETY: these identity queries have no pointer arguments.
    let (current_uid, current_gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    if current_uid != 0 && (options.uid != current_uid || options.gid != current_gid) {
        return Err(invalid(
            "nonroot installation requires the current uid and gid",
        ));
    }

    let mut package = VerifiedPackage::read(&options.package)?;
    let mut destinations = vec![&options.prefix, &options.config, &options.state];
    if let Some(unit) = &options.systemd_unit {
        destinations.push(unit);
    }

    for destination in &destinations {
        normalized_absolute(destination)?;
        if destination.starts_with(&options.package) || options.package.starts_with(destination) {
            return Err(invalid("package and destination paths must not overlap"));
        }

        check_destination(destination, options.uid)?;
    }

    for (index, left) in destinations.iter().enumerate() {
        for right in destinations.iter().skip(index + 1) {
            if left.starts_with(right) || right.starts_with(left) {
                return Err(invalid(
                    "prefix, config, state and unit paths must not overlap",
                ));
            }
        }
    }

    let repeat = match open_directory(&options.prefix, Some(options.uid)) {
        Ok(_) => {
            let mut entries = fs::read_dir(&options.prefix)?;
            if entries.next().transpose()?.is_none() {
                false
            } else {
                let installed = VerifiedPackage::read(&options.prefix).map_err(|_| invalid(
                    "existing prefix differs or is unmanaged; choose a new version-specific --prefix and retain the old prefix for cleanup"))?;
                if installed.manifest_bytes != package.manifest_bytes {
                    return Err(invalid(
                        "existing prefix differs; choose a new version-specific --prefix and retain the old prefix for cleanup",
                    ));
                }

                true
            }
        }

        Err(InstallationError::Io(error)) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    let mut authored_files = Vec::new();
    if existing_file(&options.config, options.uid, None)?.is_none() {
        let example = package
            .files
            .get_mut("examples/config.toml")
            .ok_or_else(|| invalid("missing example config"))?;
        example.seek(SeekFrom::Start(0))?;
        let example = String::from_utf8(bounded_read(example, MANIFEST_LIMIT)?)
            .map_err(|_| invalid("example config must be UTF-8"))?;
        let mut config = example
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| invalid("example config must be valid TOML"))?;
        for (section, key, path) in [
            (
                "control",
                "database_path",
                options.state.join("outrider.sqlite3"),
            ),
            (
                "control",
                "auth_token_file",
                options.state.join("control.token"),
            ),
            (
                "ingress",
                "github_token_file",
                options.state.join("tokens/github.token"),
            ),
            (
                "ingress",
                "garm_token_file",
                options.state.join("tokens/garm.token"),
            ),
        ] {
            config[section][key] = toml_edit::value(path.to_string_lossy().as_ref());
        }

        authored_files.push(PlannedFile {
            path: options.config.clone(),
            bytes: config.to_string().into_bytes(),
            mode: 0o640,
            uid: current_uid,
            gid: options.gid,
        });
    }

    for directory in [&options.state, &options.state.join("tokens")] {
        match open_directory(directory, Some(options.uid)) {
            Ok(existing) => {
                let metadata = existing.metadata()?;
                if metadata.uid() != options.uid
                    || metadata.gid() != options.gid
                    || metadata.mode() & 0o7777 != 0o700
                {
                    // A fresh Docker volume is an empty root-owned directory.
                    // Adopt only empty directories already admitted as secure.
                    if fs::read_dir(directory)?.next().transpose()?.is_some() {
                        return Err(invalid(
                            "populated state and token directories must have requested uid/gid and mode 0700",
                        ));
                    }
                }
            }

            Err(InstallationError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    let token_path = options.state.join("control.token");
    if let Some(existing) = existing_file(&token_path, options.uid, Some(0o600))? {
        let metadata = existing.metadata()?;
        if metadata.uid() != options.uid || metadata.gid() != options.gid || metadata.len() == 0 {
            return Err(invalid(
                "existing control token must be nonempty and owned by the requested uid/gid",
            ));
        }
    } else {
        let mut random = [0_u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        authored_files.push(PlannedFile {
            path: token_path.clone(),
            bytes: format!("{}\n", hex::encode(random)).into_bytes(),
            mode: 0o600,
            uid: options.uid,
            gid: options.gid,
        });
    }

    if let Some(unit_path) = &options.systemd_unit {
        let template = package
            .files
            .get_mut("packaging/outrider.service")
            .ok_or_else(|| invalid("missing systemd unit"))?;
        template.seek(SeekFrom::Start(0))?;
        let template = String::from_utf8(bounded_read(template, MANIFEST_LIMIT)?)
            .map_err(|_| invalid("unit template must be UTF-8"))?;
        let rendered = render_unit(&template, options)?;
        if let Some(mut existing) = existing_file(unit_path, current_uid, Some(0o644))? {
            if bounded_read(&mut existing, MANIFEST_LIMIT)? != rendered.as_bytes() {
                return Err(invalid(
                    "existing systemd unit differs; preserve it or select a new --systemd-unit path",
                ));
            }
        } else {
            authored_files.push(PlannedFile {
                path: unit_path.clone(),
                bytes: rendered.into_bytes(),
                mode: 0o644,
                uid: current_uid,
                gid: current_gid,
            });
        }
    }

    // The entire package, destination tree, and authored output are now planned.
    let mut transaction = InstallationTransaction {
        entries: Vec::new(),
        adopted_directories: Vec::new(),
        committed: false,
    };
    if !repeat {
        transaction.directory(&options.prefix, current_uid, current_gid, 0o755)?;
        for entry in &package.manifest.files {
            let destination = options.prefix.join(&entry.path);
            let source = package
                .files
                .get_mut(&entry.path)
                .ok_or_else(|| invalid("verified package member disappeared"))?;
            source.seek(SeekFrom::Start(0))?;
            transaction.publish(
                &destination,
                source,
                entry.mode,
                current_uid,
                current_gid,
                Some(&entry.sha256),
            )?;
        }

        transaction.publish(
            &options.prefix.join(MANIFEST_NAME),
            &mut package.manifest_bytes.as_slice(),
            0o644,
            current_uid,
            current_gid,
            None,
        )?;
    }

    transaction.state_directory(&options.state, options.uid, options.gid)?;
    transaction.state_directory(&options.state.join("tokens"), options.uid, options.gid)?;
    for entry in authored_files {
        if entry.path == options.config {
            transaction.directory(
                entry
                    .path
                    .parent()
                    .ok_or_else(|| invalid("config requires parent directory"))?,
                current_uid,
                options.gid,
                0o750,
            )?;
        }

        transaction.publish(
            &entry.path,
            &mut entry.bytes.as_slice(),
            entry.mode,
            entry.uid,
            entry.gid,
            None,
        )?;
    }

    transaction.committed = true;
    Ok(InstallReport {
        prefix: options.prefix.clone(),
        config: options.config.clone(),
        state: options.state.clone(),
        control_token: token_path,
        systemd_unit: options.systemd_unit.clone(),
    })
}

fn invalid(message: &str) -> InstallationError {
    InstallationError::Invalid(message.to_owned())
}

fn normalized_absolute(path: &Path) -> Result<(), InstallationError> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid("paths must be UTF-8"))?;
    if !path.is_absolute()
        || path == Path::new("/")
        || text.contains('\\')
        || text.chars().any(char::is_control)
        || text.contains("//")
        || text.ends_with('/')
        || text.split('/').any(|part| part == "." || part == "..")
    {
        return Err(invalid(
            "paths must be normalized absolute paths without control characters",
        ));
    }

    Ok(())
}

fn bounded_read(file: &mut File, maximum: u64) -> Result<Vec<u8>, InstallationError> {
    let mut bytes = Vec::new();
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(invalid("file exceeds read limit"));
    }

    Ok(bytes)
}

fn systemd_path(path: &Path, environment_expansion: bool) -> Result<String, InstallationError> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid("systemd paths must be UTF-8"))?;
    let mut escaped = String::from("\"");
    for byte in text.bytes() {
        match byte {
            b'%' => escaped.push_str("%%"),
            b'$' if environment_expansion => escaped.push_str("$$"),
            b'"' => escaped.push_str("\\\""),
            b'\\' => escaped.push_str("\\\\"),
            b' '..=b'~' => escaped.push(char::from(byte)),
            _ => escaped.push_str(&format!("\\x{byte:02x}")),
        }
    }

    escaped.push('"');
    Ok(escaped)
}

fn render_unit(template: &str, options: &InstallOptions) -> Result<String, InstallationError> {
    let mut rendered = String::new();
    let mut found = BTreeSet::new();
    for line in template.lines() {
        let replacement = if line.starts_with("ExecStart=") {
            if !found.insert("ExecStart") {
                return Err(invalid("unit template has a duplicate service directive"));
            }

            format!(
                "ExecStart={} serve --config {}",
                systemd_path(&options.prefix.join("bin/outrider"), true)?,
                systemd_path(&options.config, true)?
            )
        } else if line.starts_with("User=") {
            if !found.insert("User") {
                return Err(invalid("unit template has a duplicate service directive"));
            }

            format!("User={}", options.uid)
        } else if line.starts_with("Group=") {
            if !found.insert("Group") {
                return Err(invalid("unit template has a duplicate service directive"));
            }

            format!("Group={}", options.gid)
        } else if line.starts_with("WorkingDirectory=") {
            format!("WorkingDirectory={}", systemd_path(&options.state, false)?)
        } else if line.starts_with("ReadWritePaths=") {
            format!("ReadWritePaths={}", systemd_path(&options.state, false)?)
        } else if line.starts_with("StateDirectory=") || line.starts_with("StateDirectoryMode=") {
            continue;
        } else {
            line.to_owned()
        };
        rendered.push_str(&replacement);
        rendered.push('\n');
    }

    if found != BTreeSet::from(["ExecStart", "User", "Group"]) {
        return Err(invalid(
            "unit template must declare exactly one ExecStart, User and Group",
        ));
    }

    Ok(rendered)
}
