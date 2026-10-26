use std::{
    fs,
    os::unix::{ffi::OsStringExt, fs::symlink},
    sync::atomic::{AtomicU64, Ordering},
};

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct NativeFixture {
    directory: PathBuf,
    garm: PathBuf,
    child: PathBuf,
    executable: PathBuf,
}

impl NativeFixture {
    fn new() -> Result<Self, std::io::Error> {
        // Hard-link recursion checks need the executable's filesystem.
        let directory = std::env::current_exe()?.with_file_name(format!(
            "outrider-native-garm-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory)?;
        let fixture = Self {
            garm: directory.join("garm.toml"),
            child: directory.join("cloud.toml"),
            executable: directory.join("garm-external-provider"),
            directory,
        };
        fs::write(&fixture.child, "project_id = 'synthetic-project'\n")?;
        fs::write(&fixture.executable, "#!/bin/sh\nexit 0\n")?;
        fs::set_permissions(&fixture.executable, fs::Permissions::from_mode(0o700))?;
        Ok(fixture)
    }

    fn document(&self) -> String {
        format!(
            "[database]\npassphrase = 'database-secret-canary'\n\
             [jwt_auth]\nsecret = 'jwt-secret-canary'\n\
             [[provider]]\nname = 'cloud'\nprovider_type = 'external'\n\
             [provider.external]\nprovider_executable = '{}'\nconfig_file = '{}'\n",
            self.executable.display(),
            self.child.display()
        )
    }

    fn load(
        &self,
        document: &str,
        selected: &[&str],
        environment: &BTreeMap<OsString, OsString>,
    ) -> Result<BTreeMap<String, NativeProviderRegistration>, NativeGarmError> {
        fs::write(&self.garm, document).map_err(|_| NativeGarmError::ConfigurationRead)?;
        load_native_garm_providers(
            &self.garm,
            &selected
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
            "outrider",
            environment,
        )
    }
}

impl Drop for NativeFixture {
    fn drop(&mut self) {
        let _cleanup = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn imports_only_selected_native_registration() -> TestResult {
    let fixture = NativeFixture::new()?;
    let document = format!(
        "{}\n[[provider]]\nname = 'outrider'\nprovider_type = 'external'\n\
         description = 'legitimate GARM-facing Outrider registration'\n\
         disable_jit_config = true\n[provider.external]\n\
         provider_executable = '{}'\nconfig_file = '{}'\n",
        fixture.document(),
        std::env::current_exe()?.display(),
        fixture.garm.display()
    );
    let imported = fixture.load(&document, &["cloud"], &BTreeMap::new())?;
    assert_eq!(imported.len(), 1);
    let registration = &imported["cloud"];
    assert_eq!(registration.name, "cloud");
    assert_eq!(registration.executable_path, fixture.executable);
    assert_eq!(registration.configuration_path, fixture.child);
    assert!(registration.inherited_environment.is_empty());
    assert!(!format!("{imported:?}").contains("secret-canary"));
    Ok(())
}

#[test]
fn executable_override_wins_and_directory_fallback_works() -> TestResult {
    let fixture = NativeFixture::new()?;
    let overridden = format!(
        "{}provider_dir = 'relative-and-ignored'\n",
        fixture.document()
    );
    assert_eq!(
        fixture.load(&overridden, &["cloud"], &BTreeMap::new())?["cloud"].executable_path,
        fixture.executable
    );
    let fallback = fixture.document().replace(
        &format!("provider_executable = '{}'", fixture.executable.display()),
        &format!("provider_dir = '{}'", fixture.directory.display()),
    );
    assert_eq!(
        fixture.load(&fallback, &["cloud"], &BTreeMap::new())?["cloud"].executable_path,
        fixture.executable
    );
    Ok(())
}

#[test]
fn selected_native_type_interface_and_reference_are_validated() -> TestResult {
    let fixture = NativeFixture::new()?;
    assert_eq!(
        fixture.load(&fixture.document(), &["missing"], &BTreeMap::new()),
        Err(NativeGarmError::UnknownProvider)
    );
    assert_eq!(
        fixture.load(&fixture.document(), &["cloud", "cloud"], &BTreeMap::new()),
        Err(NativeGarmError::DuplicateSelection)
    );
    for provider_type in ["lxd", "aws", ""] {
        let document = fixture
            .document()
            .replace("'external'", &format!("'{provider_type}'"));
        assert_eq!(
            fixture.load(&document, &["cloud"], &BTreeMap::new()),
            Err(NativeGarmError::UnsupportedProviderType)
        );
    }

    for interface in ["", "v0.1.0", "v0.1.1", "v7"] {
        let document = format!("{}interface_version = '{interface}'\n", fixture.document());
        let imported = fixture.load(&document, &["cloud"], &BTreeMap::new());
        if matches!(interface, "" | "v0.1.0") {
            assert_eq!(imported?.len(), 1);
        } else {
            assert_eq!(imported, Err(NativeGarmError::UnsupportedInterfaceVersion));
        }
    }

    let duplicates = format!("{}\n[[provider]]\nname = 'cloud'\n", fixture.document());
    assert_eq!(
        fixture.load(&duplicates, &["cloud"], &BTreeMap::new()),
        Err(NativeGarmError::DuplicateProvider)
    );
    Ok(())
}

#[test]
fn requires_absolute_existing_regular_native_files() -> TestResult {
    let fixture = NativeFixture::new()?;
    for invalid in [
        "".to_owned(),
        "relative.toml".to_owned(),
        fixture.directory.display().to_string(),
        fixture.directory.join("missing").display().to_string(),
    ] {
        let document = fixture
            .document()
            .replace(&fixture.child.display().to_string(), &invalid);
        assert_eq!(
            fixture.load(&document, &["cloud"], &BTreeMap::new()),
            Err(NativeGarmError::InvalidProviderConfiguration)
        );
    }

    let missing_config = fixture.document().replace(
        &format!("config_file = '{}'\n", fixture.child.display()),
        "",
    );
    assert_eq!(
        fixture.load(&missing_config, &["cloud"], &BTreeMap::new()),
        Err(NativeGarmError::InvalidProviderConfiguration)
    );
    for invalid in [
        "relative-binary".to_owned(),
        fixture.directory.display().to_string(),
        fixture.directory.join("missing").display().to_string(),
    ] {
        let document = fixture
            .document()
            .replace(&fixture.executable.display().to_string(), &invalid);
        assert_eq!(
            fixture.load(&document, &["cloud"], &BTreeMap::new()),
            Err(NativeGarmError::InvalidExecutable)
        );
    }

    fs::write(&fixture.child, "")?;
    assert_eq!(
        fixture.load(&fixture.document(), &["cloud"], &BTreeMap::new()),
        Err(NativeGarmError::InvalidProviderConfiguration)
    );
    fs::write(&fixture.child, "project_id = 'synthetic-project'\n")?;
    fs::set_permissions(&fixture.executable, fs::Permissions::from_mode(0o600))?;
    assert_eq!(
        fixture.load(&fixture.document(), &["cloud"], &BTreeMap::new()),
        Err(NativeGarmError::InvalidExecutable)
    );
    Ok(())
}

#[test]
fn rejects_own_provider_name_and_executable_file_aliases() -> TestResult {
    let fixture = NativeFixture::new()?;
    let own_name = fixture
        .document()
        .replace("name = 'cloud'", "name = 'outrider'");
    assert_eq!(
        fixture.load(&own_name, &["outrider"], &BTreeMap::new()),
        Err(NativeGarmError::RecursiveProvider)
    );
    let own_executable = std::env::current_exe()?;
    let symlink_path = fixture.directory.join("symlink-provider");
    let hardlink_path = fixture.directory.join("hardlink-provider");
    symlink(&own_executable, &symlink_path)?;
    fs::hard_link(&own_executable, &hardlink_path)?;
    for executable in [&own_executable, &symlink_path, &hardlink_path] {
        let document = fixture.document().replace(
            &fixture.executable.display().to_string(),
            &executable.display().to_string(),
        );
        assert_eq!(
            fixture.load(&document, &["cloud"], &BTreeMap::new()),
            Err(NativeGarmError::RecursiveProvider)
        );
    }

    Ok(())
}

#[test]
fn resolves_native_prefixes_without_returning_environment_values() -> TestResult {
    let fixture = NativeFixture::new()?;
    let environment = BTreeMap::from([
        (
            OsString::from("AWS_KEY"),
            OsString::from("credential-secret-canary"),
        ),
        (
            OsString::from("AWS_REGION"),
            OsString::from("region-secret-canary"),
        ),
        (OsString::from("PATH"), OsString::from("path-secret-canary")),
        (
            OsString::from("GARM_TOKEN"),
            OsString::from("garm-secret-canary"),
        ),
        (
            OsString::from("GARMENT"),
            OsString::from("excluded-secret-canary"),
        ),
    ]);
    let document = format!(
        "{}environment_variables = ['AWS_', 'AWS_KEY', 'ABSENT_', 'GARM']\n",
        fixture.document()
    );
    let imported = fixture.load(&document, &["cloud"], &environment)?;
    assert_eq!(
        imported["cloud"].inherited_environment,
        ["AWS_KEY", "AWS_REGION"]
    );
    assert!(!format!("{imported:?}").contains("secret-canary"));
    let document = format!("{}environment_variables = ['']\n", fixture.document());
    assert_eq!(
        fixture.load(&document, &["cloud"], &environment)?["cloud"].inherited_environment,
        ["AWS_KEY", "AWS_REGION", "PATH"]
    );
    let document = format!(
        "{}environment_variables = ['ABSENT_']\n",
        fixture.document()
    );
    assert!(
        fixture.load(&document, &["cloud"], &environment)?["cloud"]
            .inherited_environment
            .is_empty()
    );
    Ok(())
}

#[test]
fn rejects_only_selected_non_utf8_environment_names() -> TestResult {
    let fixture = NativeFixture::new()?;
    let environment = BTreeMap::from([
        (
            OsString::from_vec(b"AWS_\xff".to_vec()),
            OsString::from("secret-canary"),
        ),
        (
            OsString::from_vec(b"GARM_\xff".to_vec()),
            OsString::from("secret-canary"),
        ),
    ]);
    let document = format!("{}environment_variables = ['AWS_']\n", fixture.document());
    assert_eq!(
        fixture.load(&document, &["cloud"], &environment),
        Err(NativeGarmError::InvalidEnvironment)
    );
    let document = format!(
        "{}environment_variables = ['UNMATCHED_', 'GARM']\n",
        fixture.document()
    );
    assert!(
        fixture.load(&document, &["cloud"], &environment)?["cloud"]
            .inherited_environment
            .is_empty()
    );
    Ok(())
}

#[test]
fn parse_errors_never_expose_native_document_secrets() -> TestResult {
    let fixture = NativeFixture::new()?;
    let error = fixture.load("[database]\npassphrase = 'database-secret-canary'\n[jwt_auth]\nsecret = 'jwt-secret-canary\n", &["cloud"], &BTreeMap::new()).expect_err("malformed native TOML must fail");
    assert_eq!(error, NativeGarmError::InvalidConfiguration);
    assert!(!format!("{error:?} {error}").contains("secret-canary"));
    Ok(())
}
