use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fmt::Debug,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use outrider::{
    garm::{BootstrapInstance, ProviderRequest},
    providers::{
        LocalFailure, ProcessLimits, ProviderBinding, ProviderError, ProviderRegistry,
        ProviderRequirements, ProviderResult, PurchasingCapability,
    },
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

struct TestDirectory {
    path: PathBuf,
}

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

fn required<T, E: Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("unexpected test error: {error:?}"),
    }
}

fn required_error<T, E: Debug>(result: Result<T, E>) -> E {
    match result {
        Err(error) => error,
        Ok(_) => panic!("expected test error"),
    }
}

fn required_some<T>(value: Option<T>) -> T {
    match value {
        Some(value) => value,
        None => panic!("expected test value"),
    }
}

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "outrider-provider-contract-{}-{}",
            std::process::id(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        required(fs::create_dir_all(&path));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _cleanup_result = fs::remove_dir_all(&self.path);
    }
}

fn fixture_path() -> PathBuf {
    outrider_xtask::provider_fixture::build_executable()
}

fn sha256(path: &Path) -> String {
    hex::encode(Sha256::digest(required(fs::read(path))))
}

fn fixture_binding(directory: &TestDirectory) -> ProviderBinding {
    let executable_path = fixture_path();
    let configuration_path = directory.path().join("provider.toml");
    required(fs::write(&configuration_path, "fixture = true\n"));
    ProviderBinding {
        id: "aws-test-account-us-test-1".into(),
        cloud: "aws".into(),
        account: "test-account".into(),
        location: "us-test-1".into(),
        executable_sha256: sha256(&executable_path),
        configuration_sha256: sha256(&configuration_path),
        executable_path,
        configuration_path,
        source_revision: "0123456789abcdef0123456789abcdef01234567".into(),
        expected_version: "fixture-build-1".into(),
        inherited_environment: vec![
            "OUTRIDER_FIXTURE_MODE".into(),
            "OUTRIDER_RECORD".into(),
            "OUTRIDER_LAUNCH_LOG".into(),
            "OUTRIDER_MARKER".into(),
            "OUTRIDER_PID_FILE".into(),
            "SAFE_CREDENTIAL".into(),
        ],
        supported_os: BTreeSet::from(["linux".into()]),
        supported_architectures: BTreeSet::from(["x86_64".into()]),
        purchasing_capabilities: BTreeSet::from([
            PurchasingCapability::OnDemand,
            PurchasingCapability::Spot,
        ]),
    }
}

fn fixture_registry(
    binding: ProviderBinding,
    mode: &str,
    directory: &TestDirectory,
    limits: ProcessLimits,
) -> ProviderRegistry {
    let environment = BTreeMap::from([
        (
            OsString::from("OUTRIDER_FIXTURE_MODE"),
            OsString::from(mode),
        ),
        (
            OsString::from("OUTRIDER_RECORD"),
            directory.path().join("record.json").into_os_string(),
        ),
        (
            OsString::from("OUTRIDER_LAUNCH_LOG"),
            directory.path().join("launch.log").into_os_string(),
        ),
        (
            OsString::from("OUTRIDER_MARKER"),
            directory.path().join("marker").into_os_string(),
        ),
        (
            OsString::from("OUTRIDER_PID_FILE"),
            directory.path().join("child.pid").into_os_string(),
        ),
        (
            OsString::from("SAFE_CREDENTIAL"),
            OsString::from("credential-value"),
        ),
        (
            OsString::from("AMBIENT_CREDENTIAL"),
            OsString::from("ambient-secret"),
        ),
        (
            OsString::from("GARM_AMBIENT_SECRET"),
            OsString::from("ambient-garm-secret"),
        ),
    ]);
    required(ProviderRegistry::new(vec![binding], environment, limits))
}

fn requirements() -> ProviderRequirements {
    ProviderRequirements {
        os: "linux".into(),
        architecture: "x86_64".into(),
        purchasing: PurchasingCapability::OnDemand,
    }
}

fn request(command: &str) -> ProviderRequest {
    let bootstrap: Option<BootstrapInstance> = if command == "CreateInstance" {
        Some(required(serde_json::from_value(json!({
            "name": "runner-synthetic",
            "ca-cert-bundle": "AAH/",
            "arch": "x86_64",
            "os_type": "linux",
            "pool_id": "a7539ead-5271-40ac-8c55-be99d41b7ae0"
        }))))
    } else {
        None
    };

    ProviderRequest {
        command: command.into(),
        interface_version: "v1.1-request-must-not-select-child".into(),
        controller_id: "e2967645-7ea5-4b22-b08a-f885ca171393".into(),
        pool_id: matches!(command, "CreateInstance" | "ListInstances")
            .then(|| "a7539ead-5271-40ac-8c55-be99d41b7ae0".into()),
        instance_id: matches!(
            command,
            "GetInstance" | "DeleteInstance" | "StartInstance" | "StopInstance"
        )
        .then(|| "i-synthetic-001".into()),
        bootstrap,
        pool_extra_specs: None,
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(4)
}

fn error_text(error: &ProviderError) -> String {
    format!("{error} {error:?}")
}

#[tokio::test]
async fn pinned_process_configuration() {
    let directory = TestDirectory::new();
    let binding = fixture_binding(&directory);
    let registry = fixture_registry(
        binding.clone(),
        "normal",
        &directory,
        ProcessLimits::default(),
    );
    assert!(matches!(
        required(registry
            .invoke("aws-test-account-us-test-1", &request("GetVersion"), None, deadline())
            .await),
        ProviderResult::Version(version) if version == "fixture-build-1"
    ));

    let mut relative = binding.clone();
    relative.executable_path = PathBuf::from("relative-provider");
    assert!(matches!(
        ProviderRegistry::new(vec![relative], BTreeMap::new(), ProcessLimits::default()),
        Err(ProviderError::Local(LocalFailure::InvalidRegistry))
    ));

    let duplicate = ProviderRegistry::new(
        vec![binding.clone(), binding.clone()],
        BTreeMap::new(),
        ProcessLimits::default(),
    );
    assert!(matches!(
        duplicate,
        Err(ProviderError::Local(LocalFailure::InvalidRegistry))
    ));

    let mut reserved = binding.clone();
    reserved
        .inherited_environment
        .push("GARM_AMBIENT_SECRET".into());
    assert!(matches!(
        ProviderRegistry::new(
            vec![reserved],
            BTreeMap::from([(OsString::from("GARM_AMBIENT_SECRET"), OsString::from("x"))]),
            ProcessLimits::default()
        ),
        Err(ProviderError::Local(LocalFailure::InvalidRegistry))
    ));

    let changed = TestDirectory::new();
    let changed_binding = fixture_binding(&changed);
    required(fs::write(
        &changed_binding.configuration_path,
        "fixture = false\n",
    ));
    let changed_registry = fixture_registry(
        changed_binding.clone(),
        "record",
        &changed,
        ProcessLimits::default(),
    );
    required(fs::write(
        &changed_binding.configuration_path,
        "fixture = changed\n",
    ));
    let error = required_error(
        changed_registry
            .invoke(
                &changed_binding.id,
                &request("CreateInstance"),
                Some(&requirements()),
                deadline(),
            )
            .await,
    );
    assert!(matches!(
        error,
        ProviderError::Local(LocalFailure::PinValidation)
    ));
    assert!(!changed.path().join("record.json").exists());
    assert!(!changed.path().join("launch.log").exists());

    let changed_binary_dir = TestDirectory::new();
    let mut changed_binary = fixture_binding(&changed_binary_dir);
    let copied_binary = changed_binary_dir.path().join("provider-copy");
    required(fs::copy(&changed_binary.executable_path, &copied_binary));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        required(fs::set_permissions(
            &copied_binary,
            fs::Permissions::from_mode(0o755),
        ));
    }

    changed_binary.executable_path = copied_binary;
    changed_binary.executable_sha256 = sha256(&changed_binary.executable_path);
    let changed_binary_registry = fixture_registry(
        changed_binary.clone(),
        "record",
        &changed_binary_dir,
        ProcessLimits::default(),
    );
    required(fs::write(
        &changed_binary.executable_path,
        "changed after registry pin\n",
    ));
    let error = required_error(
        changed_binary_registry
            .invoke(
                &changed_binary.id,
                &request("CreateInstance"),
                Some(&requirements()),
                deadline(),
            )
            .await,
    );
    assert!(matches!(
        error,
        ProviderError::Local(LocalFailure::PinValidation)
    ));
    assert!(!changed_binary_dir.path().join("record.json").exists());
    assert!(!changed_binary_dir.path().join("launch.log").exists());

    let mismatch = TestDirectory::new();
    let mismatch_binding = fixture_binding(&mismatch);
    let mismatch_registry = fixture_registry(
        mismatch_binding.clone(),
        "version_mismatch",
        &mismatch,
        ProcessLimits::default(),
    );
    assert!(matches!(
        mismatch_registry
            .invoke(
                &mismatch_binding.id,
                &request("CreateInstance"),
                Some(&requirements()),
                deadline()
            )
            .await,
        Err(ProviderError::Local(LocalFailure::VersionMismatch))
    ));
    assert!(!mismatch.path().join("record.json").exists());
    assert_eq!(
        required(fs::read_to_string(mismatch.path().join("launch.log"))),
        "GetVersion\n"
    );

    let unknown_directory = TestDirectory::new();
    let unknown_binding = fixture_binding(&unknown_directory);
    let unknown_registry = fixture_registry(
        unknown_binding.clone(),
        "normal",
        &unknown_directory,
        ProcessLimits::default(),
    );
    let unknown = request("GetConfigJSONSchema");
    assert!(matches!(
        unknown_registry
            .invoke(&unknown_binding.id, &unknown, None, deadline())
            .await,
        Err(ProviderError::Local(LocalFailure::UnsupportedRequest))
    ));
    assert!(!unknown_directory.path().join("launch.log").exists());

    let mut unsupported = requirements();
    unsupported.os = "macos".into();
    let unsupported_directory = TestDirectory::new();
    let unsupported_binding = fixture_binding(&unsupported_directory);
    let unsupported_registry = fixture_registry(
        unsupported_binding.clone(),
        "record",
        &unsupported_directory,
        ProcessLimits::default(),
    );
    assert!(matches!(
        unsupported_registry
            .invoke(
                &unsupported_binding.id,
                &request("CreateInstance"),
                Some(&unsupported),
                deadline()
            )
            .await,
        Err(ProviderError::Local(LocalFailure::CapabilityUnavailable))
    ));
    assert!(!unsupported_directory.path().join("launch.log").exists());

    let boundary_directory = TestDirectory::new();
    let boundary_binding = fixture_binding(&boundary_directory);
    let boundary_registry = fixture_registry(
        boundary_binding.clone(),
        "normal",
        &boundary_directory,
        ProcessLimits::default(),
    );

    let mut mismatched = request("CreateInstance");
    required_some(mismatched.bootstrap.as_mut()).name = "  ".into();
    assert!(matches!(
        boundary_registry
            .invoke(
                &boundary_binding.id,
                &mismatched,
                Some(&requirements()),
                deadline()
            )
            .await,
        Err(ProviderError::Local(LocalFailure::UnsupportedRequest))
    ));
    assert!(!boundary_directory.path().join("launch.log").exists());

    let mut mismatched = request("CreateInstance");
    required_some(mismatched.bootstrap.as_mut()).pool_id = "another-pool".into();
    assert!(matches!(
        boundary_registry
            .invoke(
                &boundary_binding.id,
                &mismatched,
                Some(&requirements()),
                deadline()
            )
            .await,
        Err(ProviderError::Local(LocalFailure::UnsupportedRequest))
    ));
    assert!(!boundary_directory.path().join("launch.log").exists());

    let mut mismatched = request("CreateInstance");
    required_some(mismatched.bootstrap.as_mut()).os_type = "macos".into();
    assert!(matches!(
        boundary_registry
            .invoke(
                &boundary_binding.id,
                &mismatched,
                Some(&requirements()),
                deadline()
            )
            .await,
        Err(ProviderError::Local(LocalFailure::UnsupportedRequest))
    ));
    assert!(!boundary_directory.path().join("launch.log").exists());

    let mut mismatched = request("CreateInstance");
    required_some(mismatched.bootstrap.as_mut()).os_arch = "arm64".into();
    assert!(matches!(
        boundary_registry
            .invoke(
                &boundary_binding.id,
                &mismatched,
                Some(&requirements()),
                deadline()
            )
            .await,
        Err(ProviderError::Local(LocalFailure::UnsupportedRequest))
    ));
    assert!(!boundary_directory.path().join("launch.log").exists());
}

#[tokio::test]
async fn bootstrap_wire_and_clean_environment() {
    let directory = TestDirectory::new();
    let binding = fixture_binding(&directory);
    let registry = fixture_registry(
        binding.clone(),
        "record",
        &directory,
        ProcessLimits::default(),
    );
    let result = required(
        registry
            .invoke(
                &binding.id,
                &request("CreateInstance"),
                Some(&requirements()),
                deadline(),
            )
            .await,
    );
    assert!(matches!(result, ProviderResult::Instance(_)));

    let record: Value = required(serde_json::from_slice(&required(fs::read(
        directory.path().join("record.json"),
    ))));
    let raw = required(STANDARD.decode(required_some(record["raw"].as_str())));
    let bootstrap: Value = required(serde_json::from_slice(&raw));
    assert_eq!(bootstrap["ca-cert-bundle"], "AAH/");
    assert_eq!(record["controller"], "e2967645-7ea5-4b22-b08a-f885ca171393");
    assert_eq!(record["pool"], "a7539ead-5271-40ac-8c55-be99d41b7ae0");
    assert_eq!(record["interface"], "v0.1.0");
    assert_eq!(
        record["config"],
        binding.configuration_path.to_string_lossy().as_ref()
    );
    assert_eq!(record["safe"], "credential-value");
    assert!(record["ambient"].is_null());
    assert!(record["ambient_garm"].is_null());
}

#[tokio::test]
async fn stock_result_shapes_and_null_inventory() {
    let directory = TestDirectory::new();
    let binding = fixture_binding(&directory);
    for (mode, command, expected_count) in
        [("normal", "ListInstances", 1), ("null", "ListInstances", 0)]
    {
        let registry =
            fixture_registry(binding.clone(), mode, &directory, ProcessLimits::default());
        match required(
            registry
                .invoke(
                    &binding.id,
                    &request(command),
                    Some(&requirements()),
                    deadline(),
                )
                .await,
        ) {
            ProviderResult::Instances(instances) => {
                assert_eq!(instances.len(), expected_count);
                assert!(
                    instances
                        .iter()
                        .all(|instance| instance.provider_fault.is_none())
                );
            }

            _ => panic!("expected list result"),
        }
    }

    let registry = fixture_registry(
        binding.clone(),
        "normal",
        &directory,
        ProcessLimits::default(),
    );
    for command in ["CreateInstance", "GetInstance"] {
        match required(
            registry
                .invoke(
                    &binding.id,
                    &request(command),
                    Some(&requirements()),
                    deadline(),
                )
                .await,
        ) {
            ProviderResult::Instance(instance) => assert!(instance.provider_fault.is_none()),
            _ => panic!("expected instance"),
        }
    }

    for command in [
        "DeleteInstance",
        "StartInstance",
        "StopInstance",
        "RemoveAllInstances",
    ] {
        assert!(matches!(
            required(
                registry
                    .invoke(
                        &binding.id,
                        &request(command),
                        Some(&requirements()),
                        deadline()
                    )
                    .await
            ),
            ProviderResult::Empty
        ));
    }
}

#[tokio::test]
async fn opaque_failure_and_redaction() {
    let directory = TestDirectory::new();
    let binding = fixture_binding(&directory);
    for (mode, command) in [
        ("exit1", "GetInstance"),
        ("exit30", "GetInstance"),
        ("exit31", "GetInstance"),
        ("null_nonzero", "GetInstance"),
    ] {
        let registry =
            fixture_registry(binding.clone(), mode, &directory, ProcessLimits::default());
        let error = required_error(
            registry
                .invoke(
                    &binding.id,
                    &request(command),
                    Some(&requirements()),
                    deadline(),
                )
                .await,
        );
        let rendered = error_text(&error);
        assert!(!rendered.contains("capacity"));
        assert!(!rendered.contains("secret=do-not-leak"));
        assert!(!rendered.contains("credential-value"));
        assert!(matches!(error, ProviderError::Child(_)));
    }

    for mode in ["malformed", "extra"] {
        let registry =
            fixture_registry(binding.clone(), mode, &directory, ProcessLimits::default());
        let error = required_error(
            registry
                .invoke(
                    &binding.id,
                    &request("CreateInstance"),
                    Some(&requirements()),
                    deadline(),
                )
                .await,
        );
        assert!(matches!(error, ProviderError::Ambiguous(_)), "{error:?}");
        assert!(!error_text(&error).contains("secret child details"));
    }

    let registry = fixture_registry(
        binding.clone(),
        "exit30",
        &directory,
        ProcessLimits::default(),
    );
    let error = required_error(
        registry
            .invoke(
                &binding.id,
                &request("CreateInstance"),
                Some(&requirements()),
                deadline(),
            )
            .await,
    );
    assert!(matches!(error, ProviderError::Ambiguous(_)));
}

#[tokio::test]
async fn bounded_process_io_and_deadline() {
    let directory = TestDirectory::new();
    let binding = fixture_binding(&directory);
    let tight_limits = ProcessLimits {
        stdin_bytes: 1024,
        stdout_bytes: 1024,
        stderr_bytes: 1024,
    };

    let registry = fixture_registry(binding.clone(), "normal", &directory, tight_limits);
    let mut huge_request = request("CreateInstance");
    required_some(huge_request.bootstrap.as_mut()).name = "x".repeat(4096);
    assert!(matches!(
        registry
            .invoke(
                &binding.id,
                &huge_request,
                Some(&requirements()),
                deadline()
            )
            .await,
        Err(ProviderError::Local(LocalFailure::InputLimit))
    ));
    assert_eq!(
        required(fs::read_to_string(directory.path().join("launch.log"))),
        "GetVersion\n"
    );

    for mode in ["stdout_flood", "stderr_flood"] {
        let registry = fixture_registry(binding.clone(), mode, &directory, tight_limits);
        let error = required_error(
            registry
                .invoke(
                    &binding.id,
                    &request("CreateInstance"),
                    Some(&requirements()),
                    deadline(),
                )
                .await,
        );
        assert!(matches!(error, ProviderError::Ambiguous(_)), "{error:?}");
        assert!(!error_text(&error).contains("xxxxxxxx"));
    }

    let hung_registry = fixture_registry(
        binding.clone(),
        "no_stdin_reader",
        &directory,
        ProcessLimits::default(),
    );
    let mut large_bootstrap = request("CreateInstance");
    required_some(large_bootstrap.bootstrap.as_mut()).name = "x".repeat(256 * 1024);
    let error = required_error(
        hung_registry
            .invoke(
                &binding.id,
                &large_bootstrap,
                Some(&requirements()),
                Instant::now() + Duration::from_millis(300),
            )
            .await,
    );
    assert!(matches!(error, ProviderError::Ambiguous(_)));

    let expired = fixture_registry(
        binding.clone(),
        "normal",
        &directory,
        ProcessLimits::default(),
    );
    assert!(matches!(
        expired
            .invoke(
                &binding.id,
                &request("GetVersion"),
                None,
                Instant::now() - Duration::from_millis(1)
            )
            .await,
        Err(ProviderError::Local(LocalFailure::Deadline))
    ));

    let marker = directory.path().join("marker");
    let pid_file = directory.path().join("child.pid");
    let cancellation_registry = fixture_registry(
        binding.clone(),
        "descendant",
        &directory,
        ProcessLimits::default(),
    );
    let child_registry = cancellation_registry.clone();
    let binding_id = binding.id.clone();
    let task = tokio::spawn(async move {
        child_registry
            .invoke(
                &binding_id,
                &request("GetInstance"),
                Some(&requirements()),
                deadline(),
            )
            .await
    });
    required(
        tokio::time::timeout(Duration::from_secs(2), async {
            while !pid_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await,
    );
    task.abort();
    let _task_result = task.await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !marker.exists(),
        "cancelled process group left a running descendant"
    );
    let process_ids = required(fs::read_to_string(pid_file));
    let provider_pid = required_some(process_ids.lines().next()).parse::<i32>();
    let provider_pid = required(provider_pid);
    // SAFETY: signal 0 only checks whether the recorded provider PID still exists.
    let result = unsafe { libc::kill(provider_pid, 0) };
    assert_eq!(result, -1, "cancelled provider process was not reaped");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}
