//! Exact package schema, integrity and static executable admission.

use super::filesystem::{open_directory, open_member};
use super::{
    FILE_LIMIT, InstallationError, MANIFEST_LIMIT, MANIFEST_NAME, TOTAL_LIMIT, bounded_read,
    invalid, normalized_absolute,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::Path,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageManifest {
    pub schema_version: u32,
    pub product: String,
    pub version: String,
    pub source_revision: String,
    pub host: String,
    pub files: Vec<PackageFile>,
    pub providers: Vec<PackageProvider>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageFile {
    pub path: String,
    pub sha256: String,
    pub mode: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageProvider {
    pub cloud: String,
    pub source_revision: String,
    pub expected_version: String,
    pub binary_path: String,
    pub binary_sha256: String,
    pub license_path: String,
    pub license_sha256: String,
}

pub(super) struct VerifiedPackage {
    pub(super) manifest: PackageManifest,
    pub(super) manifest_bytes: Vec<u8>,
    pub(super) files: BTreeMap<String, File>,
}

impl VerifiedPackage {
    pub(super) fn read(package: &Path) -> Result<Self, InstallationError> {
        normalized_absolute(package)?;
        let directory = open_directory(package, None)?;
        let mut manifest_file = open_member(&directory, Path::new(MANIFEST_NAME))?;
        regular_file(&manifest_file, 0o644)?;
        let manifest_bytes = bounded_read(&mut manifest_file, MANIFEST_LIMIT)?;
        let manifest: PackageManifest = serde_json::from_slice(&manifest_bytes)?;
        validate_manifest(&manifest)?;
        let mut files = BTreeMap::new();
        let mut total = 0_u64;
        for entry in &manifest.files {
            let mut file = open_member(&directory, Path::new(&entry.path))?;
            let length = regular_file(&file, entry.mode)?;
            total = total
                .checked_add(length)
                .ok_or_else(|| invalid("package size overflow"))?;
            if total > TOTAL_LIMIT {
                return Err(invalid("package exceeds total size limit"));
            }

            verify_contents(&mut file, entry)?;
            files.insert(entry.path.clone(), file);
        }

        let expected = manifest
            .files
            .iter()
            .map(|entry| entry.path.clone())
            .chain([MANIFEST_NAME.to_owned()])
            .collect::<BTreeSet<_>>();
        let mut actual = BTreeSet::new();
        list_members(package, package, &mut actual)?;
        if expected != actual {
            return Err(invalid("package contains missing or unlisted files"));
        }

        Ok(Self {
            manifest,
            manifest_bytes,
            files,
        })
    }
}

fn relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn hex_value(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_manifest(manifest: &PackageManifest) -> Result<(), InstallationError> {
    if manifest.schema_version != 1
        || manifest.product != "outrider"
        || manifest.host != "linux/amd64"
        || !semantic_version(&manifest.version)
        || !hex_value(&manifest.source_revision, 40)
        || manifest.providers.len() != 3
    {
        return Err(invalid(
            "unsupported package schema, product, host, version or source revision",
        ));
    }

    let mut required = BTreeSet::from([
        "bin/outrider".to_owned(),
        "licenses/Outrider-LICENSE".to_owned(),
        "licenses/NOTICE".to_owned(),
        "licenses/Apache-2.0.txt".to_owned(),
        "examples/garm.toml".to_owned(),
        "examples/config.toml".to_owned(),
        "examples/config-minimal.toml".to_owned(),
        "examples/repository.toml".to_owned(),
        "packaging/outrider.service".to_owned(),
    ]);
    let mut clouds = BTreeSet::new();
    for provider in &manifest.providers {
        if !["aws", "azure", "gcp"].contains(&provider.cloud.as_str())
            || !clouds.insert(&provider.cloud)
            || !hex_value(&provider.source_revision, 40)
            || provider.expected_version.is_empty()
            || provider.expected_version.len() > 128
            || !provider
                .expected_version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-+_".contains(&byte))
            || provider.binary_path != format!("providers/garm-provider-{}", provider.cloud)
            || provider.license_path != format!("licenses/{}-LICENSE", provider.cloud)
            || !hex_value(&provider.binary_sha256, 64)
            || !hex_value(&provider.license_sha256, 64)
        {
            return Err(invalid("invalid or duplicate provider metadata"));
        }

        required.insert(provider.binary_path.clone());
        required.insert(provider.license_path.clone());
    }

    let mut paths = BTreeSet::new();
    for entry in &manifest.files {
        let executable = entry.path == "bin/outrider" || entry.path.starts_with("providers/");
        if !relative_path(&entry.path)
            || !paths.insert(entry.path.clone())
            || !hex_value(&entry.sha256, 64)
            || entry.mode != if executable { 0o755 } else { 0o644 }
        {
            return Err(invalid(
                "invalid, duplicate or incorrectly permissioned package path",
            ));
        }
    }

    if required != paths {
        return Err(invalid(
            "package file list must match the supported exact layout",
        ));
    }

    for provider in &manifest.providers {
        for (path, digest) in [
            (&provider.binary_path, &provider.binary_sha256),
            (&provider.license_path, &provider.license_sha256),
        ] {
            if !manifest
                .files
                .iter()
                .any(|file| &file.path == path && &file.sha256 == digest)
            {
                return Err(invalid("provider metadata does not match file digest"));
            }
        }
    }

    Ok(())
}

fn semantic_version(version: &str) -> bool {
    if version.is_empty() || version.len() > 128 {
        return false;
    }

    let (release, build) = version
        .split_once('+')
        .map_or((version, None), |(release, build)| (release, Some(build)));
    let (core, prerelease) = release
        .split_once('-')
        .map_or((release, None), |(core, prerelease)| {
            (core, Some(prerelease))
        });
    if core.split('.').count() != 3
        || !core.split('.').all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'))
        })
    {
        return false;
    }

    for (identifiers, numeric_zeroes) in [(build, true), (prerelease, false)] {
        if let Some(identifiers) = identifiers
            && !identifiers.split('.').all(|identifier| {
                !identifier.is_empty()
                    && identifier
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    && (numeric_zeroes
                        || identifier == "0"
                        || !identifier.starts_with('0')
                        || !identifier.bytes().all(|byte| byte.is_ascii_digit()))
            })
        {
            return false;
        }
    }

    true
}

fn regular_file(file: &File, mode: u32) -> Result<u64, InstallationError> {
    let metadata = file.metadata()?;
    // SAFETY: identity query has no pointer arguments.
    let uid = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || (metadata.uid() != 0 && metadata.uid() != uid)
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != mode
        || metadata.len() > FILE_LIMIT
    {
        return Err(invalid(
            "package member must be a bounded regular single-link file with its exact manifest mode",
        ));
    }

    Ok(metadata.len())
}

fn verify_contents(file: &mut File, entry: &PackageFile) -> Result<(), InstallationError> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut count = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }

        count += read as u64;
        if count > FILE_LIMIT {
            return Err(invalid("package member grew beyond read limit"));
        }

        digest.update(&buffer[..read]);
    }

    if hex::encode(digest.finalize()) != entry.sha256 {
        return Err(invalid("package member SHA256 mismatch"));
    }

    if entry.mode == 0o755 {
        file.seek(SeekFrom::Start(0))?;
        let mut header = [0_u8; 64];
        file.read_exact(&mut header)?;
        if &header[..7] != b"\x7fELF\x02\x01\x01"
            || ![0, 3].contains(&header[7])
            || u16::from_le_bytes([header[18], header[19]]) != 62
            || u32::from_le_bytes(
                header[20..24]
                    .try_into()
                    .map_err(|_| invalid("invalid ELF version"))?,
            ) != 1
            || u16::from_le_bytes([header[52], header[53]]) != 64
            || ![2, 3].contains(&u16::from_le_bytes([header[16], header[17]]))
        {
            return Err(invalid("executable must be an ELF64 Linux amd64 binary"));
        }

        let offset = u64::from_le_bytes(
            header[32..40]
                .try_into()
                .map_err(|_| invalid("invalid ELF offset"))?,
        );
        let width = u16::from_le_bytes([header[54], header[55]]);
        let count = u16::from_le_bytes([header[56], header[57]]);
        if width != 56
            || count == 0
            || count > 1024
            || offset < 64
            || offset
                .checked_add(u64::from(width) * u64::from(count))
                .is_none_or(|end| end > file.metadata().map(|metadata| metadata.len()).unwrap_or(0))
        {
            return Err(invalid("invalid ELF program header table"));
        }

        for index in 0..count {
            file.seek(SeekFrom::Start(
                offset + u64::from(index) * u64::from(width),
            ))?;
            let mut program = [0_u8; 56];
            file.read_exact(&mut program)?;
            let kind = u32::from_le_bytes(
                program[..4]
                    .try_into()
                    .map_err(|_| invalid("invalid ELF segment"))?,
            );
            if kind == 3 {
                return Err(invalid("executable must be static (no PT_INTERP)"));
            }

            if kind == 2 {
                let dynamic_offset = u64::from_le_bytes(
                    program[8..16]
                        .try_into()
                        .map_err(|_| invalid("invalid ELF segment"))?,
                );
                let length = u64::from_le_bytes(
                    program[32..40]
                        .try_into()
                        .map_err(|_| invalid("invalid ELF segment"))?,
                );
                if length == 0
                    || length % 16 != 0
                    || length > MANIFEST_LIMIT
                    || dynamic_offset.checked_add(length).is_none_or(|end| {
                        end > file.metadata().map(|metadata| metadata.len()).unwrap_or(0)
                    })
                {
                    return Err(invalid("invalid ELF dynamic table"));
                }

                file.seek(SeekFrom::Start(dynamic_offset))?;
                let mut terminated = false;
                for _ in 0..length / 16 {
                    let mut entry = [0_u8; 16];
                    file.read_exact(&mut entry)?;
                    let tag = u64::from_le_bytes(
                        entry[..8]
                            .try_into()
                            .map_err(|_| invalid("invalid ELF dynamic entry"))?,
                    );
                    if tag == 0 {
                        terminated = true;
                        break;
                    }

                    if tag == 1 {
                        return Err(invalid("executable must be static (no DT_NEEDED)"));
                    }
                }

                if !terminated {
                    return Err(invalid("unterminated ELF dynamic table"));
                }
            }
        }
    }

    Ok(())
}

fn list_members(
    root: &Path,
    path: &Path,
    files: &mut BTreeSet<String>,
) -> Result<(), InstallationError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.file_type()?;
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| invalid("invalid package member"))?
            .to_str()
            .ok_or_else(|| invalid("non-UTF8 package member"))?
            .to_owned();
        if !relative_path(&relative) || metadata.is_symlink() {
            return Err(invalid("unsafe package member"));
        }

        if metadata.is_dir() {
            if !["bin", "providers", "examples", "packaging", "licenses"]
                .contains(&relative.as_str())
            {
                return Err(invalid("unexpected package directory"));
            }

            list_members(root, &entry.path(), files)?;
        } else if metadata.is_file() {
            if files.len() >= 16 {
                return Err(invalid("too many package members"));
            }

            files.insert(relative);
        } else {
            return Err(invalid("nonregular package member"));
        }
    }

    Ok(())
}
