use super::*;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);
const KEY: &[u8] = include_bytes!("fixtures/app-test.pem");

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    directory: PathBuf,
    config: TokenRenewalConfig,
    ingress: IngressSettings,
}

impl Fixture {
    fn new(origin: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let directory = std::env::current_dir()?.join("target").join(format!(
            "authentication-fixture-{}-{}",
            std::process::id(),
            FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let config = TokenRenewalConfig {
            github_app_id: 123,
            github_installation_id: 456,
            github_private_key_file: directory.join("app.pem"),
            garm_credentials_file: directory.join("garm.json"),
        };
        write_private(&config.github_private_key_file, KEY)?;
        write_private(
            &config.garm_credentials_file,
            br#"{"username":"existing-admin","password":"private-password"}"#,
        )?;
        let ingress = IngressSettings {
            github_api_origin: format!("{origin}github/root/"),
            github_token_file: directory
                .join("github-token")
                .to_string_lossy()
                .into_owned(),
            garm_api_origin: format!("{origin}garm/root"),
            garm_token_file: directory.join("garm-token").to_string_lossy().into_owned(),
            controller_id: "test".into(),
            provider_name: "test".into(),
            pool_flavor: "test".into(),
            pool_image: "test".into(),
            runner_prefix: "test".into(),
            max_dynamic_pools: 1,
            max_runners_per_pool: 1,
            allocation_timeout_seconds: 60,
            bootstrap_timeout_minutes: 10,
            timeout_margin_seconds: 10,
            pool_ttl_seconds: 60,
            webhook_max_bytes: 100,
            repositories: BTreeMap::new(),
        };
        Ok(Self {
            directory,
            config,
            ingress,
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.directory).expect("remove authentication fixture directory");
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[derive(Clone)]
struct Reply {
    github_status: u16,
    garm_status: u16,
    github_body: String,
    garm_body: String,
    chunked: bool,
    disconnect: bool,
    github_delay: Duration,
}

impl Reply {
    fn valid() -> Self {
        let expires_at = Utc::now() + chrono::Duration::hours(1);
        Self {
            chunked: false,
            disconnect: false,
            github_delay: Duration::ZERO,
            github_status: 201,
            garm_status: 200,
            github_body: serde_json::json!({"token":"installation-secret", "expires_at":expires_at.to_rfc3339()}).to_string(),
            garm_body: serde_json::json!({"token":jwt(expires_at.timestamp())}).to_string(),
        }
    }
}

struct Server {
    origin: String,
    reply: Arc<Mutex<Reply>>,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl Server {
    async fn start() -> Result<Self, Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}/", listener.local_addr()?);
        let reply = Arc::new(Mutex::new(Reply::valid()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::clone(&reply);
        let seen = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let reply = Arc::clone(&replies);
                let requests = Arc::clone(&seen);
                tokio::spawn(Self::respond(stream, reply, requests));
            }
        });
        Ok(Self {
            origin,
            reply,
            requests,
            task,
        })
    }

    async fn respond(
        mut stream: TcpStream,
        reply: Arc<Mutex<Reply>>,
        requests: Arc<Mutex<Vec<String>>>,
    ) {
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0; 2048];
            let Ok(length) = stream.read(&mut chunk).await else {
                return;
            };
            if length == 0 {
                return;
            }

            bytes.extend_from_slice(&chunk[..length]);
            let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let header = String::from_utf8_lossy(&bytes[..end]);
            let size = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .and_then(|size| size.parse::<usize>().ok())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + size {
                break;
            }
        }

        let request = String::from_utf8_lossy(&bytes).into_owned();
        let github = request.starts_with("POST /github/root/app/installations/456/access_tokens ");
        requests.lock().expect("request lock").push(request);
        let reply = reply.lock().expect("reply lock").clone();
        let (status, body) = if github {
            (reply.github_status, reply.github_body)
        } else {
            (reply.garm_status, reply.garm_body)
        };
        if github && !reply.github_delay.is_zero() {
            tokio::time::sleep(reply.github_delay).await;
        }

        if reply.disconnect {
            return;
        }

        let response = if reply.chunked {
            format!(
                "HTTP/1.1 {status} Fixture\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
                body.len()
            )
        } else {
            format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nLocation: /must-not-follow\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        match stream.write_all(response.as_bytes()).await {
            Ok(()) => {}
            // Bounded-body and timeout tests deliberately drop their connection.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                ) => {}
            Err(error) => panic!("fixture response write failed: {error}"),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn jwt(exp: i64) -> String {
    format!(
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.{}.c2lnbmF0dXJl",
        URL_SAFE_NO_PAD.encode(serde_json::json!({"exp":exp}).to_string())
    )
}

#[tokio::test]
async fn authentication_issues_cached_tokens_and_preserves_prefixes() -> TestResult {
    let server = Server::start().await?;
    let fixture = Fixture::new(&server.origin)?;
    let mut renewal = TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await?;
    assert_eq!(
        fs::read_to_string(&fixture.ingress.github_token_file)?,
        "installation-secret"
    );
    assert!(fs::read_to_string(&fixture.ingress.garm_token_file)?.starts_with("eyJhbGci"));
    assert_eq!(
        fs::metadata(&fixture.ingress.github_token_file)?
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(renewal.refresh_due().await? > Duration::from_secs(2000));
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().any(
        |request| request.starts_with("POST /garm/root/api/v1/auth/login ")
            && request.contains("existing-admin")
            && request.contains("private-password")
    ));
    let request = requests
        .iter()
        .find(|request| request.starts_with("POST /github/"))
        .unwrap();
    let bearer = request
        .lines()
        .find_map(|line| line.strip_prefix("authorization: Bearer "))
        .unwrap();
    let payload = bearer.split('.').nth(1).unwrap();
    let claims: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload)?)?;
    assert_eq!(claims["iss"], "123");
    assert_eq!(
        claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
        600
    );
    let mut validation = jsonwebtoken::Validation::new(Algorithm::RS256);
    validation.validate_aud = false;
    // OpenSSL derives the public key from this synthetic fixture in a separate test.
    let public = include_bytes!("fixtures/app-test-public.pem");
    jsonwebtoken::decode::<serde_json::Value>(
        bearer,
        &jsonwebtoken::DecodingKey::from_rsa_pem(public)?,
        &validation,
    )?;
    Ok(())
}

#[tokio::test]
async fn authentication_partial_failure_preserves_independent_token_and_retries_later() -> TestResult
{
    let server = Server::start().await?;
    let fixture = Fixture::new(&server.origin)?;
    let mut renewal = TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await?;
    let garm = fs::read(&fixture.ingress.garm_token_file)?;
    server.reply.lock().unwrap().github_status = 401;
    renewal.github.next_attempt = 0;
    assert_eq!(renewal.refresh_due().await, Err(AuthError::Github));
    assert_eq!(fs::read(&fixture.ingress.garm_token_file)?, garm);
    assert_eq!(
        fs::read_to_string(&fixture.ingress.github_token_file)?,
        "installation-secret"
    );
    assert!((1..=30).contains(&renewal.report().next_refresh_in.as_secs()));
    assert!(renewal.refresh_due().await.is_ok());
    assert_eq!(server.requests.lock().unwrap().len(), 3);
    renewal.github.expires_at = Some(Utc::now().timestamp() - 1);
    renewal.github.next_attempt = 0;
    assert_eq!(renewal.refresh_due().await, Err(AuthError::Github));
    assert!(!Path::new(&fixture.ingress.github_token_file).exists());
    assert_eq!(fs::read(&fixture.ingress.garm_token_file)?, garm);
    assert!(renewal.report().github_expires_at.is_none());
    Ok(())
}

#[tokio::test]
async fn authentication_due_success_refreshes_only_due_authority() -> TestResult {
    let server = Server::start().await?;
    let fixture = Fixture::new(&server.origin)?;
    let mut renewal = TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await?;
    renewal.garm.next_attempt = 0;
    renewal.refresh_due().await?;
    assert_eq!(server.requests.lock().unwrap().len(), 3);
    Ok(())
}

#[tokio::test]
async fn authentication_lock_lifetime_and_restart() -> TestResult {
    let server = Server::start().await?;
    let fixture = Fixture::new(&server.origin)?;
    let renewal = TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await?;
    assert!(matches!(
        TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await,
        Err(AuthError::Locked)
    ));
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    drop(renewal);
    TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await?;
    assert_eq!(server.requests.lock().unwrap().len(), 4);
    Ok(())
}

#[tokio::test]
async fn authentication_rejects_invalid_or_expired_bodies_and_redirects_without_replay()
-> TestResult {
    for (github_body, status) in [
        ("private-body-password".to_owned(), 201),
        (serde_json::json!({"token":"private-token","expires_at":"bad-expiry"}).to_string(), 201),
        (serde_json::json!({"token":"private-token","expires_at":"2000-01-01T00:00:00Z"}).to_string(), 201),
        (serde_json::json!({"token":"bad token","expires_at":(Utc::now()+chrono::Duration::hours(1)).to_rfc3339()}).to_string(), 201),
        ("secret".repeat(BODY_LIMIT), 201),
        (String::new(), 302),
    ] {
        let server = Server::start().await?;
        let fixture = Fixture::new(&server.origin)?;
        server.reply.lock().unwrap().github_body = github_body;
        server.reply.lock().unwrap().github_status = status;
        let error = TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await.err().unwrap();
        assert_eq!(error.to_string(), "GitHub installation token issuance failed");
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    for token in [
        "bad-jwt".to_owned(),
        "e30.e30.signature".to_owned(),
        jwt(Utc::now().timestamp() - 1),
        jwt(Utc::now().timestamp() + 5),
    ] {
        let server = Server::start().await?;
        let fixture = Fixture::new(&server.origin)?;
        server.reply.lock().unwrap().garm_body = serde_json::json!({"token":token}).to_string();
        assert!(matches!(
            TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await,
            Err(AuthError::Garm)
        ));
    }

    Ok(())
}

#[test]
fn authentication_rejects_origins_and_unsafe_paths() -> TestResult {
    for origin in [
        "http://example.com/",
        "https://user:password@example.com/",
        "https://@example.com/",
        "https://example.com/?secret=value",
        "https://example.com/#fragment",
        "ftp://example.com/",
    ] {
        assert_eq!(endpoint(origin, "login"), Err(AuthError::Configuration));
    }

    assert_eq!(
        endpoint("https://example.com/prefix", "login")?.as_str(),
        "https://example.com/prefix/login"
    );
    assert!(endpoint("http://[::1]/prefix/", "login").is_ok());
    assert!(TokenFile::open(Path::new("relative")).is_err());
    assert!(TokenFile::open(Path::new("/root/token/")).is_err());
    assert!(TokenFile::open(Path::new("/tmp/secret")).is_err());
    let fixture = Fixture::new("http://127.0.0.1:1/")?;
    let symlink_path = fixture.directory.join("symlink");
    symlink(&fixture.config.github_private_key_file, &symlink_path)?;
    assert!(read_secret(&symlink_path).is_err());
    let hardlink = fixture.directory.join("hardlink");
    fs::hard_link(&fixture.config.github_private_key_file, &hardlink)?;
    assert!(read_secret(&hardlink).is_err());
    fs::remove_file(&hardlink)?;
    fs::set_permissions(
        &fixture.config.github_private_key_file,
        fs::Permissions::from_mode(0o644),
    )?;
    assert!(read_secret(&fixture.config.github_private_key_file).is_err());
    fs::set_permissions(&fixture.directory, fs::Permissions::from_mode(0o777))?;
    assert!(TokenFile::open(&fixture.directory.join("token")).is_err());
    Ok(())
}

#[tokio::test]
async fn authentication_rejects_invalid_key_credentials_and_overlapping_destinations() -> TestResult
{
    let fixture = Fixture::new("http://127.0.0.1:1/")?;
    write_private(
        &fixture.config.github_private_key_file,
        b"private-invalid-key",
    )?;
    assert!(matches!(
        TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await,
        Err(AuthError::Github)
    ));
    write_private(&fixture.config.github_private_key_file, KEY)?;
    write_private(
        &fixture.config.garm_credentials_file,
        br#"{"password":"private-password"}"#,
    )?;
    assert!(matches!(
        TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await,
        Err(AuthError::Garm)
    ));
    let mut ingress = fixture.ingress.clone();
    ingress.github_token_file = fixture
        .config
        .github_private_key_file
        .to_string_lossy()
        .into_owned();
    assert!(matches!(
        TokenRenewal::connect(fixture.config.clone(), &ingress).await,
        Err(AuthError::Configuration)
    ));
    ingress.github_token_file = ingress.garm_token_file.clone();
    assert!(matches!(
        TokenRenewal::connect(fixture.config.clone(), &ingress).await,
        Err(AuthError::Configuration)
    ));
    Ok(())
}

#[test]
fn authentication_publish_refuses_symlinks_hardlinks_and_keeps_atomic_mode() -> TestResult {
    let fixture = Fixture::new("http://127.0.0.1:1/")?;
    let path = Path::new(&fixture.ingress.github_token_file);
    let output = TokenFile::open(path)?;
    symlink(&fixture.config.garm_credentials_file, path)?;
    assert!(output.publish(b"new-token").is_err());
    assert!(output.invalidate().is_err());
    fs::remove_file(path)?;
    fs::hard_link(&fixture.config.garm_credentials_file, path)?;
    assert!(output.publish(b"new-token").is_err());
    fs::remove_file(path)?;
    output.publish(b"new-token")?;
    output.publish(b"replacement-token")?;
    assert_eq!(fs::read_to_string(path)?, "replacement-token");
    assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
    assert!(!fs::read_dir(&fixture.directory)?.any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".outrider-token")
    }));
    output.invalidate()?;
    assert!(!path.exists());
    Ok(())
}

#[test]
fn authentication_schedule_has_no_busy_loop_and_expiry_preempts_retry() -> TestResult {
    let fixture = Fixture::new("http://127.0.0.1:1/")?;
    let mut token = ManagedToken::new(TokenFile::open(Path::new(
        &fixture.ingress.github_token_file,
    ))?);
    let now = Utc::now().timestamp();
    token.finish(Ok(IssuedToken {
        value: "token".into(),
        expires_at: now + 60,
    }))?;
    assert_eq!(token.next_deadline(), now + 48);
    token.next_attempt = now + 300;
    assert_eq!(token.next_deadline(), now + 60);
    assert!(!token.prepare(now + 60)?);
    assert!(!Path::new(&fixture.ingress.github_token_file).exists());
    assert!(validate_token("token".into(), now + 5).is_none());
    Ok(())
}

#[test]
fn authentication_bypasses_ambient_proxy_in_owned_child() -> TestResult {
    if std::env::var_os("OUTRIDER_AUTH_PROXY_CHILD").is_some() {
        let runtime = tokio::runtime::Runtime::new()?;
        return runtime.block_on(async {
            let server = Server::start().await?;
            let fixture = Fixture::new(&server.origin)?;
            TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await?;
            assert_eq!(server.requests.lock().unwrap().len(), 2);
            Ok(())
        });
    }

    let status = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "authentication::tests::authentication_bypasses_ambient_proxy_in_owned_child",
            "--nocapture",
        ])
        .env("OUTRIDER_AUTH_PROXY_CHILD", "1")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("http_proxy", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("all_proxy", "http://127.0.0.1:9")
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .status()?;
    assert!(status.success());
    Ok(())
}

#[tokio::test]
async fn authentication_bounds_chunked_bodies_and_never_replays_ambiguous_post() -> TestResult {
    let server = Server::start().await?;
    let fixture = Fixture::new(&server.origin)?;
    {
        let mut reply = server.reply.lock().unwrap();
        reply.chunked = true;
        reply.github_body = "sensitive-body".repeat(BODY_LIMIT);
    }

    assert!(matches!(
        TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await,
        Err(AuthError::Github)
    ));
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    {
        let mut reply = server.reply.lock().unwrap();
        reply.disconnect = true;
    }

    assert!(
        TokenRenewal::connect(fixture.config.clone(), &fixture.ingress)
            .await
            .is_err()
    );
    assert_eq!(server.requests.lock().unwrap().len(), 4);
    Ok(())
}

#[tokio::test]
async fn authentication_one_publication_failure_does_not_lose_other_renewal() -> TestResult {
    let server = Server::start().await?;
    let fixture = Fixture::new(&server.origin)?;
    let mut renewal = TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await?;
    let old_expiry = renewal.report().garm_expires_at.unwrap();
    server.reply.lock().unwrap().garm_body =
        serde_json::json!({"token":jwt(old_expiry+3600)}).to_string();
    fs::remove_file(&fixture.ingress.github_token_file)?;
    symlink(
        &fixture.config.garm_credentials_file,
        &fixture.ingress.github_token_file,
    )?;
    renewal.github.next_attempt = 0;
    renewal.garm.next_attempt = 0;
    assert_eq!(renewal.refresh_due().await, Err(AuthError::PrivateFile));
    assert_eq!(renewal.report().garm_expires_at, Some(old_expiry + 3600));
    assert_eq!(server.requests.lock().unwrap().len(), 4);
    assert!(
        fs::read_to_string(&fixture.config.garm_credentials_file)?.contains("private-password")
    );
    Ok(())
}

#[tokio::test]
async fn authentication_slow_refresh_expires_other_cached_authority() -> TestResult {
    let server = Server::start().await?;
    let fixture = Fixture::new(&server.origin)?;
    let mut renewal = TokenRenewal::connect(fixture.config.clone(), &fixture.ingress).await?;
    renewal.github.next_attempt = 0;
    renewal.garm.next_attempt = Utc::now().timestamp() + 100;
    renewal.garm.expires_at = Some(Utc::now().timestamp() + 1);
    server.reply.lock().unwrap().github_delay = Duration::from_secs(2);
    assert_eq!(renewal.refresh_due().await, Err(AuthError::Github));
    assert!(!Path::new(&fixture.ingress.garm_token_file).exists());
    assert!(renewal.report().garm_expires_at.is_none());
    assert!(Path::new(&fixture.ingress.github_token_file).exists());
    Ok(())
}
