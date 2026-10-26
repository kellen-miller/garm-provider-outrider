use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use outrider::{
    bootstrap::{self, BootstrapPlatform, BootstrapProvider, BootstrapTarget},
    garm::{BootstrapInstance, ProviderRequest},
    policy::{PriceComponent, PriceQuote},
    providers::{
        LocalFailure, ProcessLimits, ProviderBinding, ProviderError, ProviderRegistry,
        ProviderRequirements, PurchasingCapability,
    },
};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> std::io::Result<Self> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "outrider-mac-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            eprintln!("could not remove Mac fixture: {error}");
        }
    }
}

fn mac_instance() -> Result<BootstrapInstance, serde_json::Error> {
    let mut instance: BootstrapInstance =
        serde_json::from_str(include_str!("acceptance/fixtures/garm-bootstrap.json"))?;
    instance.os_type = "macos".to_owned();
    instance.os_arch = "arm64".to_owned();
    instance.tools[0].os = Some("osx".to_owned());
    instance.tools[0].architecture = Some("arm64".to_owned());
    instance.tools[0].filename = Some("actions-runner-osx-arm64.tar.gz".to_owned());
    instance.tools[0].sha256_checksum = Some("a".repeat(64));
    Ok(instance)
}

fn aws_binding(directory: &TestDirectory) -> std::io::Result<ProviderBinding> {
    let executable_path = outrider_xtask::provider_fixture::build_executable();
    let executable = fs::read(&executable_path)?;
    let configuration_path = directory.path.join("provider.toml");
    fs::write(&configuration_path, "region = \"us-east-1\"\n")?;
    Ok(ProviderBinding {
        id: "aws-child".to_owned(),
        cloud: "aws".to_owned(),
        account: "synthetic-account".to_owned(),
        location: "us-east-1".to_owned(),
        executable_path,
        executable_sha256: hex::encode(Sha256::digest(executable)),
        configuration_path,
        configuration_sha256: hex::encode(Sha256::digest(b"region = \"us-east-1\"\n")),
        source_revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        expected_version: "fixture-build-1".to_owned(),
        inherited_environment: vec!["OUTRIDER_LAUNCH_LOG".to_owned()],
        supported_os: BTreeSet::from(["linux".to_owned()]),
        supported_architectures: BTreeSet::from(["amd64".to_owned()]),
        purchasing_capabilities: BTreeSet::from([PurchasingCapability::OnDemand]),
    })
}

fn component(rate: &str, included: bool) -> Result<PriceComponent, rust_decimal::Error> {
    Ok(PriceComponent {
        hourly_rate: Some(Decimal::from_str_exact(rate)?),
        minimum_commitment_seconds: 60,
        source: "AWS public price list".to_owned(),
        observed_at_unix_seconds: Some(1),
        effective_at_unix_seconds: Some(1),
        included_in_compute: included,
        exclusions: Vec::new(),
        effective_price_override: None,
    })
}

#[test]
fn unsupported_macos_fail_fast() -> Result<(), Box<dyn std::error::Error>> {
    let instance = mac_instance()?;
    let result = bootstrap::generate_provider_hook(
        &instance,
        BootstrapTarget {
            platform: BootstrapPlatform::MacosArm64,
            macos_user_data_enabled: true,
        },
        BootstrapProvider::Aws,
    );
    let Err(error) = result else {
        panic!("pinned GARM providers do not support Mac bootstrap");
    };
    assert!(
        error
            .to_string()
            .contains("macOS provider bootstrap is unsupported")
    );
    assert!(error.to_string().contains("pinned GARM providers"));
    Ok(())
}

#[tokio::test]
async fn provider_capability_gate() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let binding = aws_binding(&directory)?;
    let launch_log = directory.path.join("launches.log");
    let environment = BTreeMap::from([(
        std::ffi::OsString::from("OUTRIDER_LAUNCH_LOG"),
        launch_log.as_os_str().to_owned(),
    )]);
    let registry = ProviderRegistry::new(vec![binding], environment, ProcessLimits::default())?;
    let request = ProviderRequest {
        command: "CreateInstance".to_owned(),
        interface_version: outrider::providers::GARM_INTERFACE_VERSION.to_owned(),
        controller_id: "controller".to_owned(),
        pool_id: Some("pool".to_owned()),
        instance_id: None,
        bootstrap: Some(mac_instance()?),
        pool_extra_specs: None,
    };
    let requirements = ProviderRequirements {
        os: "macos".to_owned(),
        architecture: "arm64".to_owned(),
        purchasing: PurchasingCapability::OnDemand,
    };
    let result = registry
        .invoke(
            "aws-child",
            &request,
            Some(&requirements),
            None,
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .await;
    assert!(matches!(
        result,
        Err(ProviderError::Local(LocalFailure::CapabilityUnavailable))
    ));
    assert!(
        !launch_log.exists(),
        "unsupported platform must fail before spawn"
    );
    Ok(())
}

#[test]
fn minimum_commitment_cost_preserved() -> Result<(), rust_decimal::Error> {
    let quote = PriceQuote {
        currency: "USD".to_owned(),
        provider_request_ids: Vec::new(),
        compute: component("1.00", false)?,
        disk: component("0.01", false)?,
        licensing: component("0", true)?,
        public_address: component("0", true)?,
        new_host_minimum_commitment_seconds: Some(24 * 60 * 60),
        existing_host_remaining_commitment_seconds: None,
        observed_age_seconds: Some(0),
        included_charge_names: BTreeSet::new(),
        exclusions: vec!["taxes excluded".to_owned()],
    };
    let total = quote
        .estimated_total(3600)
        .unwrap_or_else(|_| panic!("price values are valid"));
    assert_eq!(total, Decimal::from_str_exact("24.01")?);
    Ok(())
}
