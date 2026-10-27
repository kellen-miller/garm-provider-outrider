use outrider::installation::{
    InstallOptions, PackageFile, PackageManifest, PackageProvider, inspect_package, install,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[test]
fn installed_setup_repeats_with_its_own_native_registration()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new();
    let options = fixture.options();
    install(&options)?;
    let child = fixture.root.join("aws.toml");
    write(&child, b"region='us-east-1'\nsubnet_id='subnet-fixture'\n[credentials]\ncredential_type='static'\n[credentials.static]\naccess_key_id='fixture'\nsecret_access_key='fixture'\n", 0o600);
    let native = fixture.root.join("garm.toml");
    let native_text = format!(
        "[apiserver]\nbind='127.0.0.1'\nport=9980\n\n[[provider]]\nname='aws-main'\nprovider_type='external'\n[provider.external]\ninterface_version='v0.1.0'\nprovider_executable={:?}\nconfig_file={:?}\n",
        options.prefix.join("providers/garm-provider-aws"),
        child
    );
    write(&native, native_text.as_bytes(), 0o600);
    let mut source = include_str!("../examples/config.toml").parse::<toml_edit::DocumentMut>()?;
    source.remove("azure_catalog_bindings");
    source.remove("gcp_catalog_bindings");
    let selected = source["providers"]
        .as_array_of_tables_mut()
        .ok_or("missing providers")?;
    selected.remove(2);
    selected.remove(1);
    source["garm"]["config_file"] = toml_edit::value(native.display().to_string());
    write(&options.config, source.to_string().as_bytes(), 0o600);
    let setup = outrider::setup::SetupOptions {
        config: options.config.clone(),
        package: options.prefix.clone(),
        profile: "default".to_owned(),
        register: true,
    };
    outrider::setup::setup(&setup)?;
    let configured = fs::read(&options.config)?;
    let registered = fs::read(&native)?;
    outrider::config::CentralConfig::from_toml(std::str::from_utf8(&configured)?)?
        .load_native_configuration(&std::env::vars_os().collect())?
        .normalize_profile(None, "default", None)?;
    outrider::setup::setup(&setup)?;
    assert_eq!(fs::read(&options.config)?, configured);
    assert_eq!(fs::read(&native)?, registered);
    Ok(())
}

struct Fixture {
    root: PathBuf,
    package: PathBuf,
    manifest: PackageManifest,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::current_dir()
            .unwrap_or_else(|error| panic!("current installation test directory: {error}"))
            .join("target/installation-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root)
            .unwrap_or_else(|error| panic!("create installation fixture: {error}"));
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("protect fixture: {error}"));
        let package = root.join("package");
        let mut manifest = PackageManifest {
            schema_version: 1,
            product: "outrider".into(),
            version: "1.2.3".into(),
            source_revision: "a".repeat(40),
            host: "linux/amd64".into(),
            files: Vec::new(),
            providers: Vec::new(),
        };
        for path in [
            "bin/outrider",
            "providers/garm-provider-aws",
            "providers/garm-provider-azure",
            "providers/garm-provider-gcp",
            "licenses/Outrider-LICENSE",
            "licenses/NOTICE",
            "licenses/Apache-2.0.txt",
            "licenses/aws-LICENSE",
            "licenses/azure-LICENSE",
            "licenses/gcp-LICENSE",
            "examples/garm.toml",
            "examples/config.toml",
            "examples/config-minimal.toml",
            "examples/repository.toml",
            "packaging/outrider.service",
        ] {
            let executable = path == "bin/outrider" || path.starts_with("providers/");
            let bytes = if executable {
                static_elf()
            } else if path == "packaging/outrider.service" {
                b"[Unit]\nDescription=Outrider\n[Service]\nUser=outrider\nGroup=outrider\nExecStart=/opt/outrider/bin/outrider serve --config /etc/outrider/config.toml\nWorkingDirectory=/var/lib/outrider\nStateDirectory=outrider\nStateDirectoryMode=0750\nReadWritePaths=/var/lib/outrider\n[Install]\nWantedBy=multi-user.target\n".to_vec()
            } else if path == "examples/config.toml" {
                b"[control]\n[ingress]\n".to_vec()
            } else {
                b"fixture\n".to_vec()
            };
            let mode = if executable { 0o755 } else { 0o644 };
            write(&package.join(path), &bytes, mode);
            manifest.files.push(PackageFile {
                path: path.into(),
                sha256: hex::encode(Sha256::digest(&bytes)),
                mode,
            });
        }

        for cloud in ["aws", "azure", "gcp"] {
            let binary_path = format!("providers/garm-provider-{cloud}");
            let license_path = format!("licenses/{cloud}-LICENSE");
            manifest.providers.push(PackageProvider {
                cloud: cloud.into(),
                source_revision: "b".repeat(40),
                expected_version: "bbbbbbb".into(),
                binary_sha256: manifest
                    .files
                    .iter()
                    .find(|file| file.path == binary_path)
                    .unwrap_or_else(|| panic!("fixture provider binary missing"))
                    .sha256
                    .clone(),
                license_sha256: manifest
                    .files
                    .iter()
                    .find(|file| file.path == license_path)
                    .unwrap_or_else(|| panic!("fixture provider license missing"))
                    .sha256
                    .clone(),
                binary_path,
                license_path,
            });
        }

        let fixture = Self {
            root,
            package,
            manifest,
        };
        fixture.save();
        fixture
    }

    fn save(&self) {
        write(
            &self.package.join("package-manifest.json"),
            &serde_json::to_vec_pretty(&self.manifest)
                .unwrap_or_else(|error| panic!("serialize fixture manifest: {error}")),
            0o644,
        );
    }

    fn options(&self) -> InstallOptions {
        // SAFETY: numeric identity queries have no pointer arguments.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        InstallOptions {
            package: self.package.clone(),
            prefix: self.root.join("installed"),
            config: self.root.join("config/config.toml"),
            state: self.root.join("state"),
            systemd_unit: Some(self.root.join("systemd/outrider.service")),
            uid,
            gid,
        }
    }

    fn assert_rejected_before_write(&self) {
        let options = self.options();
        assert!(install(&options).is_err());
        for path in [
            &options.prefix,
            &options.config,
            &options.state,
            options
                .systemd_unit
                .as_ref()
                .unwrap_or_else(|| panic!("fixture unit path missing")),
        ] {
            assert!(!path.exists(), "unexpected mutation: {}", path.display());
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root)
            .unwrap_or_else(|error| panic!("remove installation fixture: {error}"));
    }
}

fn write(path: &Path, bytes: &[u8], mode: u32) {
    fs::create_dir_all(
        path.parent()
            .unwrap_or_else(|| panic!("fixture parent missing")),
    )
    .unwrap_or_else(|error| panic!("create fixture parent: {error}"));
    fs::write(path, bytes).unwrap_or_else(|error| panic!("write fixture member: {error}"));
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|error| panic!("protect fixture member: {error}"));
}

fn static_elf() -> Vec<u8> {
    let mut elf = vec![0_u8; 120];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&2_u16.to_le_bytes());
    elf[18..20].copy_from_slice(&62_u16.to_le_bytes());
    elf[20..24].copy_from_slice(&1_u32.to_le_bytes());
    elf[32..40].copy_from_slice(&64_u64.to_le_bytes());
    elf[52..54].copy_from_slice(&64_u16.to_le_bytes());
    elf[54..56].copy_from_slice(&56_u16.to_le_bytes());
    elf[56..58].copy_from_slice(&1_u16.to_le_bytes());
    elf[64..68].copy_from_slice(&1_u32.to_le_bytes());
    elf
}

#[test]
fn install_preserves_authored_config_and_secret_and_repeats_exactly() {
    let fixture = Fixture::new();
    let options = fixture.options();
    write(&options.config, b"authored native paths\n", 0o600);
    let first = install(&options).unwrap();
    let token = fs::read(&first.control_token).unwrap();
    assert_eq!(token.len(), 65);
    let token_inode = fs::metadata(&first.control_token).unwrap().ino();
    assert_eq!(inspect_package(&options.prefix).unwrap().version, "1.2.3");
    let second = install(&options).unwrap();
    assert_eq!(
        fs::read(&second.config).unwrap(),
        b"authored native paths\n"
    );
    assert_eq!(fs::read(&second.control_token).unwrap(), token);
    assert_eq!(
        fs::metadata(&second.control_token).unwrap().ino(),
        token_inode
    );
    for path in [&second.state, &second.state.join("tokens")] {
        let metadata = fs::metadata(path).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o700);
        assert_eq!(metadata.uid(), options.uid);
        assert_eq!(metadata.gid(), options.gid);
    }

    assert_eq!(
        fs::metadata(&second.control_token).unwrap().mode() & 0o777,
        0o600
    );
    let report = serde_json::to_string(&second).unwrap();
    assert!(!report.contains(std::str::from_utf8(&token).unwrap().trim()));
}

#[test]
fn absent_config_is_installed_restrictively() {
    let fixture = Fixture::new();
    let options = fixture.options();
    install(&options).unwrap();
    let config = fs::read_to_string(&options.config)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    assert_eq!(
        config["control"]["database_path"].as_str().unwrap(),
        options.state.join("outrider.sqlite3").to_str().unwrap()
    );
    assert_eq!(
        config["control"]["auth_token_file"].as_str().unwrap(),
        options.state.join("control.token").to_str().unwrap()
    );
    assert_eq!(
        config["ingress"]["github_token_file"].as_str().unwrap(),
        options.state.join("tokens/github.token").to_str().unwrap()
    );
    assert_eq!(fs::metadata(&options.config).unwrap().mode() & 0o777, 0o640);
    let metadata = fs::metadata(&options.config).unwrap();
    assert_eq!(metadata.uid(), options.uid);
    assert_eq!(metadata.gid(), options.gid);
    assert_eq!(
        fs::metadata(options.config.parent().unwrap())
            .unwrap()
            .mode()
            & 0o777,
        0o750
    );
}

#[test]
fn package_hash_tamper_is_rejected_before_writes() {
    let fixture = Fixture::new();
    fs::write(fixture.package.join("examples/config.toml"), b"tampered").unwrap();
    fixture.assert_rejected_before_write();
}

#[test]
fn traversal_duplicate_unknown_and_missing_members_are_rejected() {
    for invalid_path in [
        "../escape",
        "/absolute",
        "examples\\evil",
        "examples//evil",
        "examples/./evil",
        "examples/control\n",
    ] {
        let mut fixture = Fixture::new();
        fixture.manifest.files[0].path = invalid_path.into();
        fixture.save();
        fixture.assert_rejected_before_write();
    }

    let mut duplicate = Fixture::new();
    duplicate
        .manifest
        .files
        .push(duplicate.manifest.files[0].clone());
    duplicate.save();
    duplicate.assert_rejected_before_write();
    let extra = Fixture::new();
    write(
        &extra.package.join("credential.json"),
        b"must not copy",
        0o644,
    );
    extra.assert_rejected_before_write();
    let missing = Fixture::new();
    fs::remove_file(missing.package.join("licenses/aws-LICENSE")).unwrap();
    missing.assert_rejected_before_write();
}

#[test]
fn symlink_hardlink_and_wrong_mode_members_are_rejected() {
    let symbolic = Fixture::new();
    fs::remove_file(symbolic.package.join("examples/config.toml")).unwrap();
    symlink(
        "config-minimal.toml",
        symbolic.package.join("examples/config.toml"),
    )
    .unwrap();
    symbolic.assert_rejected_before_write();
    let hard = Fixture::new();
    fs::hard_link(
        hard.package.join("examples/config.toml"),
        hard.root.join("second-link"),
    )
    .unwrap();
    hard.assert_rejected_before_write();
    let mode = Fixture::new();
    fs::set_permissions(
        mode.package.join("bin/outrider"),
        fs::Permissions::from_mode(0o775),
    )
    .unwrap();
    mode.assert_rejected_before_write();
}

#[test]
fn manifest_bounds_provider_set_and_identity_are_enforced() {
    let oversized = Fixture::new();
    fs::write(
        oversized.package.join("package-manifest.json"),
        vec![b' '; 65537],
    )
    .unwrap();
    oversized.assert_rejected_before_write();
    let mut identity = Fixture::new();
    identity.manifest.source_revision = "unknown".into();
    identity.save();
    identity.assert_rejected_before_write();
    let mut provider = Fixture::new();
    provider.manifest.providers[2] = provider.manifest.providers[0].clone();
    provider.save();
    provider.assert_rejected_before_write();
}

#[test]
fn dynamic_wrong_architecture_and_malformed_elf_are_rejected() {
    for byte in [3_u8, 2] {
        let mut fixture = Fixture::new();
        let mut elf = static_elf();
        elf[64] = byte;
        write(&fixture.package.join("bin/outrider"), &elf, 0o755);
        fixture.manifest.files[0].sha256 = hex::encode(Sha256::digest(&elf));
        fixture.save();
        fixture.assert_rejected_before_write();
    }

    let mut architecture = Fixture::new();
    let mut elf = static_elf();
    elf[18] = 183;
    write(&architecture.package.join("bin/outrider"), &elf, 0o755);
    architecture.manifest.files[0].sha256 = hex::encode(Sha256::digest(&elf));
    architecture.save();
    architecture.assert_rejected_before_write();
}

#[test]
fn changed_bundle_and_unmanaged_prefix_preserve_existing_files() {
    let mut fixture = Fixture::new();
    let options = fixture.options();
    install(&options).unwrap();
    let previous = fs::read(options.prefix.join("bin/outrider")).unwrap();
    fixture.manifest.version = "1.2.4".into();
    fixture.save();
    let error = install(&options).unwrap_err().to_string();
    assert!(error.contains("new version-specific --prefix"));
    assert_eq!(
        fs::read(options.prefix.join("bin/outrider")).unwrap(),
        previous
    );
    let unmanaged = Fixture::new();
    let options = unmanaged.options();
    write(&options.prefix.join("existing"), b"keep", 0o600);
    assert!(install(&options).is_err());
    assert_eq!(fs::read(options.prefix.join("existing")).unwrap(), b"keep");
}

#[test]
fn overlapping_relative_and_symlink_destinations_fail() {
    let fixture = Fixture::new();
    let mut options = fixture.options();
    options.config = options.prefix.join("config.toml");
    assert!(install(&options).is_err());
    assert!(!options.prefix.exists());
    options = fixture.options();
    options.state = "relative".into();
    assert!(install(&options).is_err());
    options = fixture.options();
    fs::create_dir(fixture.root.join("other")).unwrap();
    symlink(fixture.root.join("other"), &options.prefix).unwrap();
    assert!(install(&options).is_err());
    assert!(
        fs::read_dir(fixture.root.join("other"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn writable_ancestors_tmp_and_wrong_state_permissions_fail() {
    let fixture = Fixture::new();
    let mut options = fixture.options();
    options.prefix = PathBuf::from(format!(
        "/tmp/outrider-installer-reject-{}",
        std::process::id()
    ));
    assert!(install(&options).is_err());
    assert!(!options.prefix.exists());
    options = fixture.options();
    fs::create_dir(&options.state).unwrap();
    write(&options.state.join("existing"), b"preserve", 0o600);
    fs::set_permissions(&options.state, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(install(&options).is_err());
    assert!(!options.prefix.exists());
}

#[test]
fn nonroot_cannot_request_a_different_identity() {
    let fixture = Fixture::new();
    let mut options = fixture.options();
    if options.uid == 0 {
        return;
    }

    options.uid += 1;
    assert!(
        install(&options)
            .unwrap_err()
            .to_string()
            .contains("current uid and gid")
    );
    assert!(!options.prefix.exists());
}

#[test]
fn unit_paths_are_escaped_and_differing_existing_unit_is_preserved() {
    let fixture = Fixture::new();
    let mut options = fixture.options();
    options.prefix = fixture.root.join("version with %n $NAME and \"quotes\"");
    options.config = fixture.root.join("config with %n $NAME/config.toml");
    install(&options).unwrap();
    let unit_path = options.systemd_unit.as_ref().unwrap();
    let unit = fs::read_to_string(unit_path).unwrap();
    assert!(unit.contains("%%n $$NAME"));
    assert!(unit.contains("\\\"quotes\\\""));
    assert!(unit.contains(&format!("User={}\n", options.uid)));
    assert!(unit.contains(&format!("ReadWritePaths=\"{}\"", options.state.display())));
    assert!(!unit.contains("StateDirectory="));
    write(unit_path, b"authored unit", 0o644);
    assert!(install(&options).is_err());
    assert_eq!(fs::read(unit_path).unwrap(), b"authored unit");
}

#[test]
fn failed_publication_rolls_back_new_prefix_and_preserves_prior_data() {
    let fixture = Fixture::new();
    let mut options = fixture.options();
    if options.uid == 0 {
        return;
    }

    let blocked = fixture.root.join("blocked");
    fs::create_dir(&blocked).unwrap();
    write(&blocked.join("prior"), b"retain", 0o600);
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o500)).unwrap();
    options.state = blocked.join("state");
    assert!(install(&options).is_err());
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!options.prefix.exists());
    assert!(!options.config.exists());
    assert_eq!(fs::read(blocked.join("prior")).unwrap(), b"retain");
    assert!(!options.systemd_unit.unwrap().exists());
}

#[test]
fn fresh_empty_state_volume_is_adopted_and_populated_state_is_never_repaired() {
    let fixture = Fixture::new();
    let options = fixture.options();
    fs::create_dir(&options.state).unwrap();
    fs::set_permissions(&options.state, fs::Permissions::from_mode(0o755)).unwrap();
    install(&options).unwrap();
    assert_eq!(fs::metadata(&options.state).unwrap().mode() & 0o777, 0o700);
    install(&options).unwrap();
    let token = fs::read(options.state.join("control.token")).unwrap();
    fs::set_permissions(&options.state, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(install(&options).is_err());
    assert_eq!(fs::metadata(&options.state).unwrap().mode() & 0o777, 0o750);
    assert_eq!(
        fs::read(options.state.join("control.token")).unwrap(),
        token
    );
}

#[test]
fn static_pie_is_allowed_but_dynamic_dependency_is_rejected() {
    for needed in [false, true] {
        let mut fixture = Fixture::new();
        let mut elf = static_elf();
        elf.resize(136, 0);
        elf[16..18].copy_from_slice(&3_u16.to_le_bytes());
        elf[64..68].copy_from_slice(&2_u32.to_le_bytes());
        elf[72..80].copy_from_slice(&120_u64.to_le_bytes());
        elf[96..104].copy_from_slice(&16_u64.to_le_bytes());
        if needed {
            elf[120] = 1;
        }

        write(&fixture.package.join("bin/outrider"), &elf, 0o755);
        fixture.manifest.files[0].sha256 = hex::encode(Sha256::digest(&elf));
        fixture.save();
        assert_eq!(inspect_package(&fixture.package).is_ok(), !needed);
    }
}

#[test]
fn malformed_versions_and_foreign_package_ancestors_are_rejected() {
    for version in [
        "1.2.3-",
        "1.2.3+",
        "01.2.3",
        "1.2.3-alpha..1",
        "1.2.3-01",
        "1.2.3+bad+build",
    ] {
        let mut fixture = Fixture::new();
        fixture.manifest.version = version.into();
        fixture.save();
        fixture.assert_rejected_before_write();
    }

    let fixture = Fixture::new();
    fs::set_permissions(&fixture.package, fs::Permissions::from_mode(0o775)).unwrap();
    fixture.assert_rejected_before_write();
}

#[test]
fn symlink_and_hardlinked_secrets_are_rejected_without_prefix_mutation() {
    let fixture = Fixture::new();
    let options = fixture.options();
    fs::create_dir(&options.state).unwrap();
    fs::set_permissions(&options.state, fs::Permissions::from_mode(0o700)).unwrap();
    write(
        &fixture.root.join("original.token"),
        b"protected token",
        0o600,
    );
    symlink(
        fixture.root.join("original.token"),
        options.state.join("control.token"),
    )
    .unwrap();
    assert!(install(&options).is_err());
    assert!(!options.prefix.exists());
    assert_eq!(
        fs::read(fixture.root.join("original.token")).unwrap(),
        b"protected token"
    );
    fs::remove_file(options.state.join("control.token")).unwrap();
    fs::hard_link(
        fixture.root.join("original.token"),
        options.state.join("control.token"),
    )
    .unwrap();
    assert!(install(&options).is_err());
    assert!(!options.prefix.exists());
}

#[test]
fn rollback_restores_adopted_empty_state_and_removes_only_new_output() {
    let fixture = Fixture::new();
    let options = fixture.options();
    if options.uid == 0 {
        return;
    }

    fs::create_dir(&options.state).unwrap();
    fs::set_permissions(&options.state, fs::Permissions::from_mode(0o755)).unwrap();
    let unit_parent = options.systemd_unit.as_ref().unwrap().parent().unwrap();
    fs::create_dir(unit_parent).unwrap();
    write(
        &unit_parent.join("prior.service"),
        b"preserve prior unit",
        0o644,
    );
    fs::set_permissions(unit_parent, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(install(&options).is_err());
    fs::set_permissions(unit_parent, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!options.prefix.exists());
    assert!(!options.config.exists());
    assert!(fs::read_dir(&options.state).unwrap().next().is_none());
    assert_eq!(fs::metadata(&options.state).unwrap().mode() & 0o777, 0o755);
    assert_eq!(
        fs::read(unit_parent.join("prior.service")).unwrap(),
        b"preserve prior unit"
    );
}
