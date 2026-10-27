use std::{
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::Ordering,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use outrider::garm::ProviderResponse;
use serde_json::{Value, json};

use super::daemon_scenario::{BOOTSTRAP_TOKEN, INNER_TOKEN, NEXT_DIRECTORY, TestDirectory};

pub(super) struct HttpFixture {
    pub(super) address: String,
    pub(super) received: Receiver<io::Result<Vec<u8>>>,
    pub(super) release: Option<Sender<()>>,
    pub(super) worker: thread::JoinHandle<io::Result<()>>,
}

impl HttpFixture {
    pub(super) fn start(status: u16, body: String, hold_response: bool) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?.to_string();
        let (received_tx, received) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept()?;
            let request = read_http_request(&mut stream);
            received_tx
                .send(request)
                .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))?;
            if hold_response {
                released
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error.to_string()))?;
                return Ok(());
            }

            let reason = if status == 200 { "OK" } else { "Not Found" };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .map_err(io::Error::other)?;
            stream.flush()?;
            Ok(())
        });
        Ok(Self {
            address,
            received,
            release: hold_response.then_some(release),
            worker,
        })
    }

    pub(super) fn request(&self) -> io::Result<Vec<u8>> {
        self.received
            .recv_timeout(Duration::from_secs(3))
            .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error.to_string()))?
    }

    pub(super) fn finish(self) -> io::Result<()> {
        if let Some(release) = self.release {
            release
                .send(())
                .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error.to_string()))?;
        }

        self.worker
            .join()
            .map_err(|_| io::Error::other("HTTP fixture worker panicked"))??;
        Ok(())
    }
}

pub(super) fn read_http_request(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            break;
        }

        bytes.extend_from_slice(&buffer[..count]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };

        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or_default();
        if bytes.len() >= header_end + 4 + content_length {
            break;
        }
    }

    Ok(bytes)
}

pub(super) fn write_provider_config(
    directory: &TestDirectory,
    address: &str,
    timeout_seconds: u64,
) -> io::Result<PathBuf> {
    let token_path = directory.path().join("token");
    let config = directory.path().join(format!(
        "config-{}.toml",
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&token_path, INNER_TOKEN)?;
    fs::write(
        &config,
        format!(
            "version = 1\n[control]\nlisten_address = {address:?}\ndatabase_path = {:?}\nauth_token_file = {:?}\nprovider_timeout_seconds = {timeout_seconds}\n[ingress]\ngithub_api_origin = 'https://github.invalid'\ngithub_token_file = '/unused'\ngarm_api_origin = 'https://garm.invalid'\ngarm_token_file = '/unused'\ncontroller_id = 'controller-test'\nprovider_name = 'outrider'\npool_flavor = 'synthetic'\npool_image = 'synthetic'\nrunner_prefix = 'runner'\nmax_dynamic_pools = 1\nmax_runners_per_pool = 1\nallocation_timeout_seconds = 600\nbootstrap_timeout_minutes = 20\ntimeout_margin_seconds = 30\npool_ttl_seconds = 3600\nwebhook_max_bytes = 1048576\n[ingress.repositories]\n",
            directory.path().join("journal.sqlite3").to_string_lossy(),
            token_path.to_string_lossy()
        ),
    )?;
    Ok(config)
}

pub(super) fn provider_process(
    config: &Path,
    command: &str,
    interface: Option<&str>,
    instance: Option<&str>,
    pool: Option<&str>,
    extra_specs: Option<&str>,
    body: Option<&[u8]>,
) -> io::Result<Output> {
    let mut process = Command::new(env!("CARGO_BIN_EXE_outrider"));
    process
        .env("GARM_COMMAND", command)
        .env("GARM_CONTROLLER_ID", "controller-test")
        .env("GARM_PROVIDER_CONFIG_FILE", config)
        .env_remove("GARM_POOL_EXTRASPECS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(interface) = interface {
        process.env("GARM_INTERFACE_VERSION", interface);
    }

    if let Some(instance) = instance {
        process.env("GARM_INSTANCE_ID", instance);
    }

    if let Some(pool) = pool {
        process.env("GARM_POOL_ID", pool);
    }

    if let Some(extra_specs) = extra_specs {
        process.env("GARM_POOL_EXTRASPECS", extra_specs);
    }

    if body.is_some() {
        process.stdin(Stdio::piped());
    }

    let mut child = process.spawn()?;
    if let Some(body) = body {
        child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("provider stdin was not piped"))?
            .write_all(body)?;
    }

    child.wait_with_output()
}

pub(super) fn bootstrap_json() -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&json!({
        "name": "runner-test",
        "tools": [{"os":"linux", "architecture":"amd64", "download_url":"https://example.invalid/runner.tgz", "temp_download_token":BOOTSTRAP_TOKEN}],
        "repo_url":"https://github.com/example/repo",
        "callback-url":"https://example.invalid/callback",
        "metadata-url":"https://example.invalid/metadata",
        "instance-token":BOOTSTRAP_TOKEN,
        "ssh-keys":[],
        "extra_specs":{"profile":"test"},
        "github-runner-group":"default",
        "ca-cert-bundle":STANDARD.encode(b"ca-secret-canary"),
        "arch":"amd64",
        "os_type":"linux",
        "flavor":"synthetic",
        "image":"synthetic",
        "labels":["self-hosted"],
        "pool_id":"pool-test",
        "user_data_options":{"disable_updates_on_boot":false,"extra_packages":[],"enable_boot_debug":false},
        "jit_config_enabled":true
    }))
}

pub(super) fn provider_instance_json() -> Value {
    json!({
        "provider_id":"provider-instance-1",
        "name":"runner-test",
        "os_type":"linux",
        "os_name":"ubuntu",
        "os_version":"24.04",
        "os_arch":"amd64",
        "addresses":[{"address":"10.0.0.4","type":"private"}],
        "status":"running",
        "provider_fault":STANDARD.encode(b"synthetic fault")
    })
}

pub(super) fn provider_response(
    result: Option<Value>,
    error: Option<&str>,
    not_found: bool,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&ProviderResponse {
        result,
        error: error.map(str::to_owned),
        not_found,
    })
}

pub(super) fn assert_schema_fields_are_present(schema: &Value) {
    let numeric_fields = [
        &schema["$defs"]["RunnerProfilePatch"]["properties"]["maximum_total_cost"]["type"],
        &schema["$defs"]["AdministratorLimits"]["properties"]["max_total_cost"]["type"],
        &schema["$defs"]["GpuRequirementPatch"]["properties"]["model"]["type"],
        &schema["$defs"]["GpuRequirementPatch"]["properties"]["memory_gib"]["type"],
        &schema["$defs"]["DiskRequirementPatch"]["properties"]["iops"]["type"],
        &schema["$defs"]["DiskRequirementPatch"]["properties"]["throughput_mib_s"]["type"],
    ];
    for field in numeric_fields {
        assert_ne!(*field, "null");
        assert!(
            !field
                .as_array()
                .is_some_and(|types| types.iter().any(|type_name| type_name == "null"))
        );
    }
}
