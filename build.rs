use std::{env, fs, path::PathBuf, process::Command};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=OUTRIDER_VERSION");
    let root =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("manifest directory required")?);
    // Watch tracked inputs and Git metadata, not target/ or local agent evidence.
    // Package assembly independently requires the current checkout to be clean.
    // A status refresh must not modify the index this build script watches.
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .current_dir(&root)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
    };
    for argument in ["--git-dir", "--git-common-dir"] {
        if let Some(directory) = git(&["rev-parse", argument]) {
            let directory = root.join(directory.trim());
            for file in ["HEAD", "index", "packed-refs", "refs"] {
                let path = directory.join(file);
                if path.exists() {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
        }
    }

    if let Some(files) = git(&["ls-files", "-z"]) {
        for file in files.split('\0').filter(|file| !file.is_empty()) {
            println!("cargo:rerun-if-changed={}", root.join(file).display());
        }
    }

    let revision = git(&["rev-parse", "HEAD"])
        .map(|revision| revision.trim().to_owned())
        .unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain", "--untracked-files=normal"])
        .is_none_or(|status| !status.is_empty());
    assert!(
        revision == "unknown"
            || (revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "invalid source revision"
    );
    assert!(
        revision != "unknown" || env::var("PROFILE").as_deref() == Ok("debug"),
        "release builds require a Git checkout"
    );
    let version = match env::var("OUTRIDER_VERSION") {
        Ok(version) => version,
        Err(env::VarError::NotPresent) => env!("CARGO_PKG_VERSION").into(),
        Err(error) => return Err(error.into()),
    };
    assert!(valid_version(&version), "OUTRIDER_VERSION must be semver");
    let os = env::var("CARGO_CFG_TARGET_OS")?;
    let os = if os == "macos" { "darwin" } else { &os };
    let arch = env::var("CARGO_CFG_TARGET_ARCH")?;
    let arch = match arch.as_str() {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    println!("cargo:rustc-env=OUTRIDER_BUILD_VERSION={version}");
    println!("cargo:rustc-env=OUTRIDER_SOURCE_REVISION={revision}");
    println!("cargo:rustc-env=OUTRIDER_BUILD_HOST={os}/{arch}");
    println!("cargo:rustc-env=OUTRIDER_BUILD_DIRTY={dirty}");
    // Watch nested source directories, including additions/deletions.
    let mut directories = vec![root.join("src"), root.join("xtask")];
    while let Some(directory) = directories.pop() {
        println!("cargo:rerun-if-changed={}", directory.display());
        if let Ok(entries) = fs::read_dir(directory) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    directories.push(entry.path());
                }
            }
        }
    }

    Ok(())
}

fn valid_version(version: &str) -> bool {
    let (version, metadata) = version
        .split_once('+')
        .map_or((version, None), |(v, m)| (v, Some(m)));
    let (core, prerelease) = version
        .split_once('-')
        .map_or((version, None), |(v, p)| (v, Some(p)));
    let numbers: Vec<_> = core.split('.').collect();
    let numeric = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|byte| byte.is_ascii_digit())
            && (part.len() == 1 || !part.starts_with('0'))
    };
    let identifiers = |value: &str, prerelease: bool| {
        value.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && (!prerelease || !part.bytes().all(|byte| byte.is_ascii_digit()) || numeric(part))
        })
    };
    numbers.len() == 3
        && numbers.into_iter().all(numeric)
        && prerelease.is_none_or(|value| identifiers(value, true))
        && metadata.is_none_or(|value| identifiers(value, false))
}
