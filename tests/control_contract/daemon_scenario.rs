use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Output,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use outrider::{
    allocation::{AllocationService, ConnectedCatalog, catalog_configuration_fingerprint},
    cloud::aws::AwsBackend,
    config::{
        AwsCatalogBinding, AwsImageBinding, AwsNetworkBinding, ControlSettings, IngressRepository,
        IngressSettings,
    },
    daemon::build_router,
    github::IngressService,
    journal::Journal,
    providers::{ProcessLimits, ProviderBinding, ProviderRegistry, PurchasingCapability},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use super::provider_wire::{provider_process, read_http_request};

pub(super) static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
pub(super) const INNER_TOKEN: &str = "internal-auth-secret-canary-194e";
pub(super) const BOOTSTRAP_TOKEN: &str = "bootstrap-token-secret-canary-6381";

pub(super) struct TestDirectory {
    pub(super) path: PathBuf,
}

impl TestDirectory {
    pub(super) fn new() -> io::Result<Self> {
        let count = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "outrider-control-{}-{nonce}-{count}",
                std::process::id()
            ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            eprintln!("could not remove temporary test directory: {error}");
        }
    }
}

pub(super) struct TestDaemon {
    pub(super) config: PathBuf,
    pub(super) database: PathBuf,
    pub(super) address: String,
    pub(super) shutdown: Option<oneshot::Sender<()>>,
    pub(super) worker: Option<thread::JoinHandle<io::Result<()>>>,
    pub(super) identity_fixture: Option<AwsIdentityFixture>,
}

pub(super) struct AwsIdentityFixture {
    pub(super) endpoint: String,
    pub(super) worker: Option<thread::JoinHandle<io::Result<()>>>,
}

impl TestDaemon {
    pub(super) fn start(directory: &TestDirectory) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?.to_string();
        listener.set_nonblocking(true)?;
        let database = directory.path().join("journal.sqlite3");
        let identity_fixture = AwsIdentityFixture::start()?;
        let (config, app) =
            build_test_services(directory, &database, &address, &identity_fixture.endpoint)?;
        let (shutdown, stopped) = oneshot::channel();
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(io::Error::other)?;
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(listener).map_err(io::Error::other)?;
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        drop(stopped.await);
                    })
                    .await
                    .map_err(io::Error::other)
            })
        });
        let daemon = Self {
            config,
            database,
            address,
            shutdown: Some(shutdown),
            worker: Some(worker),
            identity_fixture: Some(identity_fixture),
        };
        daemon.wait_ready()?;
        Ok(daemon)
    }

    pub(super) fn wait_ready(&self) -> io::Result<()> {
        for _ in 0..100 {
            if daemon_ready(&self.address) {
                return Ok(());
            }

            thread::sleep(Duration::from_millis(50));
        }

        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "daemon did not become ready",
        ))
    }

    pub(super) fn provider(
        &self,
        command: &str,
        instance: Option<&str>,
        body: Option<&[u8]>,
    ) -> io::Result<Output> {
        provider_process(
            &self.config,
            command,
            None,
            instance,
            Some("pool-test"),
            None,
            body,
        )
    }

    pub(super) fn stop(&mut self) -> io::Result<String> {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.send(()).unwrap_or(());
        }
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| io::Error::other("daemon worker panicked"))??;
        }
        if let Some(mut fixture) = self.identity_fixture.take() {
            fixture.finish()?;
        }

        Ok(String::new())
    }
}

impl AwsIdentityFixture {
    pub(super) fn start() -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept()?;
            let _request = read_http_request(&mut stream)?;
            let body = "<GetCallerIdentityResponse><GetCallerIdentityResult><Account>synthetic-account</Account><Arn>arn:aws:iam::synthetic-account:user/test</Arn><UserId>test</UserId></GetCallerIdentityResult></GetCallerIdentityResponse>";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )?;
            stream.flush()
        });
        Ok(Self {
            endpoint,
            worker: Some(worker),
        })
    }

    pub(super) fn finish(&mut self) -> io::Result<()> {
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| io::Error::other("AWS identity fixture panicked"))??;
        }
        Ok(())
    }
}

pub(super) fn build_test_services(
    directory: &TestDirectory,
    database: &Path,
    address: &str,
    aws_endpoint: &str,
) -> io::Result<(PathBuf, axum::Router)> {
    let token_path = directory.path().join("token");
    let github_token_path = directory.path().join("github-token");
    let garm_token_path = directory.path().join("garm-token");
    fs::write(&token_path, INNER_TOKEN)?;
    fs::write(&github_token_path, "synthetic-github-token")?;
    fs::write(&garm_token_path, "synthetic-garm-token")?;
    let provider_configuration_path = directory.path().join("provider.toml");
    let provider_configuration = "region = \"us-east-1\"\nsubnet_id = \"subnet-123\"\n[credentials]\ncredential_type = \"static\"\n[credentials.static]\naccess_key_id = \"AKIDTESTCONTROL\"\nsecret_access_key = \"secret-test-only\"\n";
    fs::write(&provider_configuration_path, provider_configuration)?;
    let executable_path = outrider_xtask::provider_fixture::build_executable();
    let binding = ProviderBinding {
        id: "aws-child".to_owned(),
        cloud: "aws".to_owned(),
        account: "synthetic-account".to_owned(),
        location: "us-east-1".to_owned(),
        executable_sha256: hex::encode(Sha256::digest(fs::read(&executable_path)?)),
        configuration_sha256: hex::encode(Sha256::digest(provider_configuration.as_bytes())),
        executable_path,
        configuration_path: provider_configuration_path,
        source_revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        expected_version: "fixture-build-1".to_owned(),
        inherited_environment: Vec::new(),
        supported_os: BTreeSet::from(["linux".to_owned()]),
        supported_architectures: BTreeSet::from(["amd64".to_owned()]),
        purchasing_capabilities: BTreeSet::from([PurchasingCapability::OnDemand]),
    };
    let mut config = outrider::config::CentralConfig::from_toml(include_str!(
        "../../examples/config-minimal.toml"
    ))
    .and_then(|source| source.load_native_configuration(&std::collections::BTreeMap::new()))
    .map_err(io::Error::other)?;
    config.control = Some(ControlSettings {
        listen_address: address.to_owned(),
        database_path: database.display().to_string(),
        auth_token_file: token_path.display().to_string(),
        provider_timeout_seconds: 15,
    });
    config.ingress = Some(IngressSettings {
        github_api_origin: "https://api.github.invalid/".to_owned(),
        github_token_file: github_token_path.display().to_string(),
        garm_api_origin: "http://127.0.0.1:1/".to_owned(),
        garm_token_file: garm_token_path.display().to_string(),
        controller_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        provider_name: "outrider".to_owned(),
        pool_flavor: "m7i.xlarge".to_owned(),
        pool_image: "ubuntu-24".to_owned(),
        runner_prefix: "runner".to_owned(),
        max_dynamic_pools: 10,
        max_runners_per_pool: 10,
        allocation_timeout_seconds: 600,
        bootstrap_timeout_minutes: 20,
        timeout_margin_seconds: 30,
        pool_ttl_seconds: 3600,
        webhook_max_bytes: 1024 * 1024,
        repositories: BTreeMap::from([(
            "example/repo".to_owned(),
            IngressRepository {
                garm_repository_id: "22222222-2222-4222-8222-222222222222".to_owned(),
                webhook_secret_file: token_path.display().to_string(),
                allow_forks: false,
                template_id: None,
            },
        )]),
    });
    config.provider_bindings = Some(vec![binding.clone()]);
    config.aws_catalog_bindings = Some(vec![AwsCatalogBinding {
        id: "aws-east".to_owned(),
        provider_binding_id: binding.id.clone(),
        account_id: binding.account.clone(),
        region: binding.location.clone(),
        pricing_location: "US East (N. Virginia)".to_owned(),
        credential_reference: format!("provider-binding:{}:configuration", binding.id),
        locations: BTreeMap::from([(
            "us-east-1".to_owned(),
            AwsNetworkBinding {
                availability_zone: "us-east-1a".to_owned(),
                subnet_id: "subnet-123".to_owned(),
                security_group_ids: vec!["sg-123".to_owned()],
            },
        )]),
        images: BTreeMap::from([(
            "ubuntu-24".to_owned(),
            AwsImageBinding {
                image_id: "ami-test".to_owned(),
                os: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                capabilities: BTreeSet::from(["docker".to_owned()]),
                windows_license_included: false,
                cloud_init: true,
                ec2_launch_agent: false,
            },
        )]),
        storage_classes: BTreeMap::from([("gp3".to_owned(), "gp3".to_owned())]),
    }]);
    let config_path = directory.path().join("daemon.toml");
    let mut config_value = serde_json::to_value(&config).map_err(io::Error::other)?;
    remove_null_values(&mut config_value);
    fs::write(
        &config_path,
        super::serialize_native_central(&config_value)
            .map_err(|error| io::Error::other(error.to_string()))?,
    )?;
    let config = super::load_fixture_central(&config_path)
        .map_err(|error| io::Error::other(error.to_string()))?;

    let aws_catalog = config
        .aws_catalog_bindings
        .as_ref()
        .and_then(|bindings| bindings.first())
        .ok_or_else(|| io::Error::other("AWS catalog is not configured"))?;
    let mut settings = AwsBackend::settings_from_bindings(aws_catalog, &binding, false)
        .map_err(io::Error::other)?;
    settings.ec2_endpoint_url = Some(aws_endpoint.to_owned());
    settings.pricing_endpoint_url = Some(aws_endpoint.to_owned());
    settings.sts_endpoint_url = Some(aws_endpoint.to_owned());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;
    let backend = runtime.block_on(async {
        let shared = aws_config::defaults(BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .credentials_provider(Credentials::new(
                "AKIDTESTCONTROL",
                "secret-test-only",
                None,
                None,
                "control-contract-test",
            ))
            .load()
            .await;
        let ec2 = aws_sdk_ec2::Client::from_conf(
            aws_sdk_ec2::config::Builder::from(&shared)
                .endpoint_url(aws_endpoint)
                .build(),
        );
        let pricing = aws_sdk_pricing::Client::from_conf(
            aws_sdk_pricing::config::Builder::from(&shared)
                .region(aws_config::Region::new("us-east-1"))
                .endpoint_url(aws_endpoint)
                .build(),
        );
        let sts = aws_sdk_sts::Client::from_conf(
            aws_sdk_sts::config::Builder::from(&shared)
                .endpoint_url(aws_endpoint)
                .build(),
        );
        AwsBackend::with_clients(
            settings,
            ec2,
            pricing,
            sts,
            tokio::time::Instant::now() + Duration::from_secs(10),
        )
        .await
        .map_err(io::Error::other)
    })?;
    let journal = Arc::new(Journal::open(database).map_err(io::Error::other)?);
    let fingerprint =
        catalog_configuration_fingerprint(&config, "aws", "aws-east").map_err(io::Error::other)?;
    let environment = std::env::vars_os().collect::<BTreeMap<OsString, OsString>>();
    let registry = ProviderRegistry::new(
        vec![binding.clone()],
        environment.clone(),
        ProcessLimits::default(),
    )
    .map_err(io::Error::other)?;
    let catalogs = vec![ConnectedCatalog::aws(
        backend,
        fingerprint,
        "aws-east".to_owned(),
        binding.id,
        binding.account,
        binding.location,
    )];
    let allocation = Arc::new(
        AllocationService::new(
            config.clone(),
            journal.clone(),
            catalogs,
            registry,
            environment,
        )
        .map_err(io::Error::other)?,
    );
    let ingress = Arc::new(IngressService::new(config, journal).map_err(io::Error::other)?);
    let app = build_router(
        INNER_TOKEN.as_bytes().to_vec(),
        allocation,
        ingress,
        hex::encode(Sha256::digest(fs::read(&config_path)?)),
    )
    .map_err(io::Error::other)?;
    Ok((config_path, app))
}

pub(super) fn remove_null_values(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                remove_null_values(value);
            }
        }
        Value::Object(values) => {
            values.retain(|_, value| !value.is_null());
            for value in values.values_mut() {
                remove_null_values(value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

pub(super) fn daemon_ready(address: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect(address) else {
        return false;
    };

    let mut response = String::new();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .and_then(|()| stream.read_to_string(&mut response))
        .is_ok()
        && response.contains("200 OK")
        && response.contains("ready")
}
