use std::{collections::BTreeMap, fs, sync::Arc};

use hmac::{Hmac, KeyInit, Mac};
use outrider::{
    config::{CentralConfig, ConfigPatch, IngressRepository},
    github::IngressService,
    journal::Journal,
};
use sha2::Sha256;

use super::daemon_scenario::TestDirectory;

#[tokio::test]
async fn public_listener_exposes_only_health_and_signed_webhooks()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let github_token = directory.path().join("github-token");
    let garm_token = directory.path().join("garm-token");
    let secret = directory.path().join("webhook-secret");
    fs::write(&github_token, "github-fixture")?;
    fs::write(&garm_token, "garm-fixture")?;
    fs::write(&secret, "webhook-fixture")?;
    let mut ingress = CentralConfig::from_toml(include_str!("../../examples/config.toml"))?
        .ingress
        .ok_or("missing fixture ingress")?;
    ingress.controller_id = "11111111-1111-4111-8111-111111111111".to_owned();
    ingress.github_api_origin = "http://127.0.0.1:9/".to_owned();
    ingress.garm_api_origin = "http://127.0.0.1:9/".to_owned();
    ingress.github_token_file = github_token.display().to_string();
    ingress.garm_token_file = garm_token.display().to_string();
    ingress.repositories = BTreeMap::from([(
        "1".to_owned(),
        IngressRepository {
            garm_repository_id: "22222222-2222-4222-8222-222222222222".to_owned(),
            webhook_secret_file: secret.display().to_string(),
            allow_forks: false,
            template_id: None,
        },
    )]);
    let journal = Arc::new(Journal::open(&directory.path().join("journal.sqlite3"))?);
    let service = Arc::new(IngressService::new(
        ConfigPatch {
            version: Some(1),
            ingress: Some(ingress),
            ..ConfigPatch::default()
        },
        journal.clone(),
    )?);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        axum::serve(listener, outrider::daemon::build_webhook_router(service)).await
    });
    let client = reqwest::Client::builder().no_proxy().build()?;
    assert!(
        client
            .get(format!("{origin}/health"))
            .send()
            .await?
            .status()
            .is_success()
    );
    for path in ["/v1/provider", "/v1/explain", "/v1/inventory", "/v1/status"] {
        let response = client
            .post(format!("{origin}{path}"))
            .bearer_auth("even-a-valid-control-token")
            .json(&serde_json::json!({}))
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    }

    let body = br#"{"repository":{"id":1}}"#;
    let rejected = client
        .post(format!("{origin}/v1/webhooks/github"))
        .header("x-github-event", "ping")
        .header("x-github-delivery", "public-invalid")
        .body(body.to_vec())
        .send()
        .await?;
    assert!(!rejected.status().is_success());
    assert!(journal.pending_deliveries(10)?.is_empty());
    let mut signature = Hmac::<Sha256>::new_from_slice(b"webhook-fixture")?;
    signature.update(body);
    let accepted = client
        .post(format!("{origin}/v1/webhooks/github"))
        .header("x-github-event", "ping")
        .header("x-github-delivery", "public-valid")
        .header(
            "x-hub-signature-256",
            format!("sha256={}", hex::encode(signature.finalize().into_bytes())),
        )
        .body(body.to_vec())
        .send()
        .await?;
    assert_eq!(accepted.status(), reqwest::StatusCode::ACCEPTED);
    assert_eq!(journal.pending_deliveries(10)?.len(), 1);
    server.abort();
    Ok(())
}

#[test]
fn online_doctor_rejects_a_daemon_running_an_older_config() -> Result<(), Box<dyn std::error::Error>>
{
    use super::daemon_scenario::{INNER_TOKEN, TestDaemon};
    use std::os::unix::fs::PermissionsExt;
    let directory = TestDirectory::new()?;
    let mut daemon = TestDaemon::start(&directory)?;
    fs::set_permissions(&daemon.config, fs::Permissions::from_mode(0o600))?;
    let original = fs::read_to_string(&daemon.config)?;
    fs::write(
        &daemon.config,
        format!("{original}\n# a new operator revision\n"),
    )?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let error = runtime.block_on(async {
        let client = reqwest::Client::builder().no_proxy().build()?;
        let endpoint = format!("http://{}/v1/status", daemon.address);
        assert_eq!(
            client.post(&endpoint).send().await?.status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        let status: serde_json::Value = client
            .post(endpoint)
            .bearer_auth(INNER_TOKEN)
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(
            status["configuration_sha256"],
            hex::encode(<Sha256 as sha2::Digest>::digest(original.as_bytes()))
        );
        let result = outrider::setup::doctor(&outrider::setup::DoctorOptions {
            config: daemon.config.clone(),
            profile: "linux-large".to_owned(),
            online: true,
        })
        .await;
        Ok::<_, Box<dyn std::error::Error>>(result.err().ok_or("stale daemon was accepted")?)
    })?;
    assert!(matches!(error, outrider::setup::SetupError::StaleDaemon));
    daemon.stop()?;
    Ok(())
}
