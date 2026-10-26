use super::*;
use serde_json::{Value, json};

struct Fixture {
    _temporary: tempfile::TempDir,
    source: PathBuf,
    options: Options,
    revision: String,
}

impl Fixture {
    fn new() -> Result<Self> {
        let temporary = tempfile::tempdir()?;
        let directory = temporary.path().canonicalize()?;
        let source = directory.join("source");
        let bundle = directory.join("bundle");
        fs::create_dir_all(source.join("providers"))?;
        fs::create_dir_all(bundle.join("bin"))?;
        fs::create_dir_all(bundle.join("licenses"))?;
        for (input, _) in ASSETS {
            let destination = source.join(input);
            fs::create_dir_all(destination.parent().unwrap())?;
            fs::copy(root().join(input), destination)?;
        }

        let mut pins: Value =
            serde_json::from_slice(&fs::read(root().join("providers/manifest.json"))?)?;
        let mut records = Vec::new();
        for pin in pins["providers"].as_array_mut().unwrap() {
            let cloud = pin["cloud"].as_str().unwrap().to_owned();
            let binary = format!("bin/garm-provider-{cloud}");
            fs::write(bundle.join(&binary), elf())?;
            set_mode(&bundle.join(&binary), 0o755)?;
            let license = bundle.join(format!("licenses/{cloud}-LICENSE"));
            fs::write(&license, format!("{cloud} fixture license\n"))?;
            let license_hash = sha256(&license)?;
            pin["license"]["sha256"] = json!(license_hash);
            records.push(json!({
                "cloud": cloud, "host": HOST, "repository": pin["fork_repository"],
                "source_revision": pin["source_revision"], "source_license_sha256": license_hash,
                "go_version": "1.27.3", "binary": binary,
                "binary_sha256": sha256(&bundle.join(binary))?, "get_version_stdout": pin["expected_version"],
            }));
        }

        fs::write(
            source.join("providers/manifest.json"),
            serde_json::to_vec(&pins)?,
        )?;
        fs::write(
            bundle.join("bundle-manifest.json"),
            serde_json::to_vec(&json!({
                "manifest_version": 1, "interface_version": "v0.1.0", "host": HOST,
                "go_version": "1.27.3", "qualification_scope": "source build and offline GetVersion only", "providers": records,
            }))?,
        )?;
        let binary = directory.join("outrider");
        fs::write(&binary, elf())?;
        set_mode(&binary, 0o755)?;
        for args in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Package Tests",
                "-c",
                "user.email=package@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        ] {
            ensure!(
                Command::new("git")
                    .args(args)
                    .current_dir(&source)
                    .status()?
                    .success(),
                "fixture Git failed"
            );
        }

        let revision = clean_revision(&source)?;
        Ok(Self {
            _temporary: temporary,
            source,
            revision,
            options: Options {
                binary,
                provider_bundle: bundle,
                output: directory.join("package"),
                version: "1.2.3-rc.1+build.2".into(),
            },
        })
    }

    fn edit_bundle(&self, edit: impl FnOnce(&mut Value)) -> Result<()> {
        let path = self.options.provider_bundle.join("bundle-manifest.json");
        let mut value = serde_json::from_slice(&fs::read(&path)?)?;
        edit(&mut value);
        fs::write(path, serde_json::to_vec(&value)?)?;
        Ok(())
    }
}

fn elf() -> Vec<u8> {
    let mut bytes = vec![0; 120];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16] = 2;
    bytes[18] = 62;
    bytes[32] = 64;
    bytes[54] = 56;
    bytes[56] = 1;
    bytes[64] = 1;
    bytes
}

#[test]
fn package_files_hashes_modes_and_manifest_are_deterministic() -> Result<()> {
    let mut fixture = Fixture::new()?;
    assemble(
        &fixture.options,
        &fixture.source,
        &fixture.revision,
        &sha256(&fixture.options.binary)?,
    )?;
    let first = fs::read(fixture.options.output.join("package-manifest.json"))?;
    let manifest: PackageManifest = serde_json::from_slice(&first)?;
    assert_eq!(manifest.files.len(), 15);
    assert_eq!(manifest.providers.len(), 3);
    assert_eq!(manifest.source_revision, fixture.revision);
    assert_eq!(manifest.version, fixture.options.version);
    let expected: BTreeSet<_> = manifest
        .files
        .iter()
        .map(|file| file.path.clone())
        .chain(["package-manifest.json".into()])
        .collect();
    let actual: BTreeSet<_> = inventory(&fixture.options.output)?
        .into_iter()
        .filter(|path| fixture.options.output.join(path).is_file())
        .collect();
    assert_eq!(actual, expected);
    for file in &manifest.files {
        let path = fixture.options.output.join(&file.path);
        assert_eq!(sha256(&path)?, file.sha256);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, file.mode);
        }
    }

    fixture.options.output.set_file_name("second");
    fs::create_dir(&fixture.options.output)?;
    assemble(
        &fixture.options,
        &fixture.source,
        &fixture.revision,
        &sha256(&fixture.options.binary)?,
    )?;
    assert_eq!(
        first,
        fs::read(fixture.options.output.join("package-manifest.json"))?
    );
    Ok(())
}

#[test]
fn package_rejects_tampering_missing_and_unknown_inputs() -> Result<()> {
    for fault in [
        "digest",
        "license",
        "missing",
        "extra",
        "source",
        "version",
        "path",
        "host",
        "duplicate",
        "missing-license",
        "mode",
    ] {
        let fixture = Fixture::new()?;
        match fault {
            "digest" => fs::write(
                fixture
                    .options
                    .provider_bundle
                    .join("bin/garm-provider-aws"),
                b"tampered",
            )?,
            "license" => fs::write(
                fixture.options.provider_bundle.join("licenses/aws-LICENSE"),
                b"tampered",
            )?,
            "missing" => fs::remove_file(
                fixture
                    .options
                    .provider_bundle
                    .join("bin/garm-provider-gcp"),
            )?,
            "missing-license" => {
                fs::remove_file(fixture.options.provider_bundle.join("licenses/gcp-LICENSE"))?
            }
            "extra" => fs::write(
                fixture.options.provider_bundle.join("credentials"),
                b"secret",
            )?,
            "source" => fixture.edit_bundle(|value| {
                value["providers"][0]["source_revision"] = json!("f".repeat(40))
            })?,
            "version" => fixture.edit_bundle(|value| {
                value["providers"][0]["get_version_stdout"] = json!("wrong")
            })?,
            "path" => fixture
                .edit_bundle(|value| value["providers"][0]["binary"] = json!("../outrider"))?,
            "host" => fixture.edit_bundle(|value| value["host"] = json!("darwin/arm64"))?,
            "duplicate" => fixture.edit_bundle(|value| {
                let duplicate = value["providers"][0].clone();
                value["providers"].as_array_mut().unwrap().push(duplicate);
            })?,
            "mode" => set_mode(
                &fixture
                    .options
                    .provider_bundle
                    .join("bin/garm-provider-aws"),
                0o644,
            )?,
            _ => unreachable!(),
        }

        assert!(
            assemble(
                &fixture.options,
                &fixture.source,
                &fixture.revision,
                &sha256(&fixture.options.binary)?
            )
            .is_err(),
            "{fault}"
        );
        assert!(!fixture.options.output.exists());
    }

    Ok(())
}

#[test]
fn package_leaves_existing_output_and_cleans_staging_on_failure() -> Result<()> {
    let fixture = Fixture::new()?;
    fs::create_dir(&fixture.options.output)?;
    fs::write(fixture.options.output.join("keep"), b"retain")?;
    assert!(
        assemble(
            &fixture.options,
            &fixture.source,
            &fixture.revision,
            &sha256(&fixture.options.binary)?
        )
        .is_err()
    );
    assert_eq!(fs::read(fixture.options.output.join("keep"))?, b"retain");
    fs::remove_file(fixture.options.output.join("keep"))?;
    fs::remove_file(fixture.source.join("packaging/outrider.service"))?;
    assert!(
        assemble(
            &fixture.options,
            &fixture.source,
            &fixture.revision,
            &sha256(&fixture.options.binary)?
        )
        .is_err()
    );
    assert!(fs::read_dir(&fixture.options.output)?.next().is_none());
    assert!(
        fs::read_dir(fixture.options.output.parent().unwrap())?.all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".outrider-package-"))
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn package_rejects_symlinks_in_bundle_and_output() -> Result<()> {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new()?;
    let binary = fixture
        .options
        .provider_bundle
        .join("bin/garm-provider-aws");
    fs::remove_file(&binary)?;
    symlink(&fixture.options.binary, binary)?;
    assert!(
        assemble(
            &fixture.options,
            &fixture.source,
            &fixture.revision,
            &sha256(&fixture.options.binary)?
        )
        .is_err()
    );
    symlink(&fixture.source, &fixture.options.output)?;
    assert!(
        assemble(
            &fixture.options,
            &fixture.source,
            &fixture.revision,
            &sha256(&fixture.options.binary)?
        )
        .is_err()
    );
    assert!(fixture.source.join("LICENSE").exists());
    Ok(())
}

#[test]
fn package_rejects_wrong_binary_identity_and_malformed_semver() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut identity = BuildInfo {
        version: fixture.options.version.clone(),
        source_revision: fixture.revision.clone(),
        host: HOST.into(),
        dirty: false,
    };
    validate_identity(&identity, &fixture.revision, &fixture.options.version)?;
    for version in [
        "1.2",
        "v1.2.3",
        "1.2.3; echo bad",
        "01.2.3",
        "1.2.3-01",
        "1.2.3+",
        "1.2.3\n",
    ] {
        assert!(validate_identity(&identity, &fixture.revision, version).is_err());
    }

    identity.dirty = true;
    assert!(validate_identity(&identity, &fixture.revision, &fixture.options.version).is_err());
    identity.dirty = false;
    identity.source_revision = "unknown".into();
    assert!(validate_identity(&identity, &fixture.revision, &fixture.options.version).is_err());
    identity.source_revision = "a".repeat(40);
    assert!(validate_identity(&identity, &fixture.revision, &fixture.options.version).is_err());
    identity.source_revision = fixture.revision.clone();
    identity.version = "1.0.0".into();
    assert!(validate_identity(&identity, &fixture.revision, &fixture.options.version).is_err());
    Ok(())
}

#[test]
fn package_rejects_dynamic_or_wrong_architecture_elf() -> Result<()> {
    let fixture = Fixture::new()?;
    static_elf(&fixture.options.binary)?;
    let mut bytes = elf();
    bytes.resize(192, 0);
    bytes[16] = 3;
    bytes[56] = 2;
    bytes[120] = 2;
    bytes[128] = 176;
    bytes[152] = 16;
    fs::write(&fixture.options.binary, &bytes)?;
    static_elf(&fixture.options.binary)?;
    bytes[176] = 1;
    fs::write(&fixture.options.binary, &bytes)?;
    assert!(static_elf(&fixture.options.binary).is_err());
    let mut bytes = elf();
    bytes[64] = 3;
    fs::write(&fixture.options.binary, &bytes)?;
    assert!(static_elf(&fixture.options.binary).is_err());
    bytes[64] = 1;
    bytes[18] = 183;
    fs::write(&fixture.options.binary, bytes)?;
    assert!(static_elf(&fixture.options.binary).is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn package_metadata_executes_with_bounded_output() -> Result<()> {
    let fixture = Fixture::new()?;
    assert_eq!(
        bounded_output(Command::new("sh").args(["-c", "printf metadata"]))?,
        "metadata"
    );
    assert!(bounded_output(Command::new("sh").args(["-c", "exit 3"])).is_err());
    assert!(bounded_output(Command::new("sh").args(["-c", "yes excessive"])).is_err());
    let start = Instant::now();
    assert!(bounded_output(Command::new("sh").args(["-c", "sleep 10"])).is_err());
    assert!(start.elapsed() < Duration::from_secs(8));
    assert!(!fixture.options.output.exists());
    Ok(())
}

#[test]
fn package_build_identity_refreshes_across_source_and_head_changes() -> Result<()> {
    let fixture = Fixture::new()?;
    fs::create_dir(fixture.source.join("src"))?;
    fs::copy(root().join("build.rs"), fixture.source.join("build.rs"))?;
    fs::write(
        fixture.source.join("Cargo.toml"),
        "[package]\nname = \"package-build-identity\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(
        fixture.source.join("src/main.rs"),
        r#"fn main() {
    println!("{{\"version\":\"{}\",\"source_revision\":\"{}\",\"host\":\"{}\",\"dirty\":{}}}",
        env!("OUTRIDER_BUILD_VERSION"), env!("OUTRIDER_SOURCE_REVISION"),
        env!("OUTRIDER_BUILD_HOST"), env!("OUTRIDER_BUILD_DIRTY"));
}
"#,
    )?;
    ensure!(
        Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(&fixture.source)
            .status()?
            .success(),
        "fixture lockfile"
    );
    let commit = || -> Result<()> {
        ensure!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(&fixture.source)
                .status()?
                .success(),
            "fixture add"
        );
        ensure!(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=Package Tests",
                    "-c",
                    "user.email=package@example.invalid",
                    "commit",
                    "-qm",
                    "metadata"
                ])
                .current_dir(&fixture.source)
                .status()?
                .success(),
            "fixture commit"
        );
        Ok(())
    };
    commit()?;
    let target = fixture.source.parent().unwrap().join("identity-target");
    let build = |version: &str| {
        Command::new("cargo")
            .args(["run", "--locked", "--offline", "--quiet", "--target-dir"])
            .arg(&target)
            .current_dir(&fixture.source)
            .env_remove("CARGO_BUILD_TARGET")
            .env("OUTRIDER_VERSION", version)
            .output()
    };
    let output = build("1.2.3")?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first: BuildInfo = serde_json::from_slice(&output.stdout)?;
    assert!(!first.dirty);
    assert_eq!(first.version, "1.2.3");
    assert_eq!(first.source_revision, clean_revision(&fixture.source)?);
    let main = fixture.source.join("src/main.rs");
    let mut contents = fs::read_to_string(&main)?;
    contents.push('\n');
    fs::write(&main, contents)?;
    let output = build("1.2.3")?;
    ensure!(output.status.success(), "dirty fixture build");
    let dirty: BuildInfo = serde_json::from_slice(&output.stdout)?;
    assert!(dirty.dirty);
    commit()?;
    let output = build("1.2.4-rc.1+local")?;
    ensure!(output.status.success(), "committed fixture build");
    let updated: BuildInfo = serde_json::from_slice(&output.stdout)?;
    assert!(!updated.dirty);
    assert_ne!(updated.source_revision, first.source_revision);
    assert_eq!(updated.source_revision, clean_revision(&fixture.source)?);
    assert_eq!(updated.version, "1.2.4-rc.1+local");
    for invalid in ["v1.2.3", "1.2.3\ncargo:rustc-env=bad", "1.2.3-01"] {
        assert!(!build(invalid)?.status.success());
    }

    fs::remove_dir_all(fixture.source.join(".git"))?;
    let output = build("1.2.3")?;
    ensure!(output.status.success(), "debug build outside Git");
    let unknown: BuildInfo = serde_json::from_slice(&output.stdout)?;
    assert_eq!(unknown.source_revision, "unknown");
    assert!(unknown.dirty);
    Ok(())
}
