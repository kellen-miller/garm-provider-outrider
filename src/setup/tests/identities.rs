use super::ProtectedDirectory;
use crate::{
    config::{CentralConfig, IngressRepository, IngressSettings},
    setup::{SetupError, adoption::resolve_identities},
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};

const CONTROLLER: &str = "5bccf673-f389-4e5c-a579-6e63d88d8249";
const REPOSITORY: &str = "95d79f0f-2280-47a5-b80d-e4c6f490b267";
const OTHER_REPOSITORY: &str = "96447c7a-126c-48aa-bc4b-d50572c4676d";

#[derive(Clone)]
struct IdentityApi {
    routes: Arc<Mutex<BTreeMap<String, Value>>>,
    requests: Arc<Mutex<Vec<ReadRequest>>>,
    statuses: Arc<Mutex<BTreeMap<String, StatusCode>>>,
}

#[derive(Debug)]
struct ReadRequest {
    method: Method,
    path: String,
    authorization: String,
}

struct IdentityFixture {
    directory: ProtectedDirectory,
    settings: IngressSettings,
    api: IdentityApi,
    server: tokio::task::JoinHandle<()>,
}

impl IdentityFixture {
    async fn new() -> Self {
        let directory = ProtectedDirectory::new();
        let github_token = directory.file("github-token", "github-bearer-canary\n");
        let garm_token = directory.file("garm-token", "garm-bearer-canary\n");
        let secret = directory.file("webhook-secret", "existing-secret-canary");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        // Field names follow GARM v0.2.1 params.Repository, ForgeCredentials,
        // ForgeEndpoint and ControllerInfo. Extra fields are ordinary API output.
        let routes = BTreeMap::from([
            (
                "/api/v1/controller".into(),
                json!({"controller_id": CONTROLLER, "hostname": "existing-garm", "metadata_url": "https://garm.example/metadata"}),
            ),
            (
                "/api/v1/repositories".into(),
                json!([{"id": REPOSITORY, "owner": "Owner", "name": "Repo", "credentials_id": 17, "agent_mode": false, "endpoint": {"api_base_url": origin}, "pool": []}]),
            ),
            (
                "/api/v1/github/credentials".into(),
                json!([{"id": 17, "name": "existing-app", "auth-type": "app", "forge_type": "github", "endpoint": {"name": "existing-forge", "api_base_url": origin}}]),
            ),
            (
                "/repos/Owner/Repo".into(),
                json!({"id": 4242, "full_name": "Owner/Repo", "private": true}),
            ),
            (
                "/repos/Owner/Repo/actions/runs?per_page=1".into(),
                json!({"total_count": 0, "workflow_runs": []}),
            ),
        ]);
        let api = IdentityApi {
            routes: Arc::new(Mutex::new(routes)),
            requests: Arc::new(Mutex::new(Vec::new())),
            statuses: Arc::new(Mutex::new(BTreeMap::new())),
        };
        let router = Router::new()
            .fallback(any(read_identity))
            .with_state(api.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let settings = IngressSettings {
            github_api_origin: origin.clone(),
            github_token_file: github_token.to_str().unwrap().into(),
            garm_api_origin: origin,
            garm_token_file: garm_token.to_str().unwrap().into(),
            controller_id: String::new(),
            provider_name: "outrider".into(),
            pool_flavor: "managed".into(),
            pool_image: "linux".into(),
            runner_prefix: "outrider".into(),
            max_dynamic_pools: 4,
            max_runners_per_pool: 1,
            allocation_timeout_seconds: 300,
            bootstrap_timeout_minutes: 5,
            timeout_margin_seconds: 30,
            pool_ttl_seconds: 600,
            webhook_max_bytes: 65536,
            repositories: BTreeMap::from([(
                "owner/repo".into(),
                IngressRepository {
                    garm_repository_id: String::new(),
                    webhook_secret_file: secret.to_str().unwrap().into(),
                    allow_forks: false,
                    template_id: Some(9),
                },
            )]),
        };
        Self {
            directory,
            settings,
            api,
            server,
        }
    }

    fn change(&self, path: &str, update: impl FnOnce(&mut Value)) {
        update(self.api.routes.lock().unwrap().get_mut(path).unwrap());
    }

    fn assert_only_authenticated_reads(&self) {
        let requests = self.api.requests.lock().unwrap();
        assert!(!requests.is_empty());
        assert!(requests.len() <= 16);
        for request in requests.iter() {
            assert_eq!(request.method, Method::GET);
            assert_eq!(
                request.authorization,
                if request.path.starts_with("/api/v1/") {
                    "Bearer garm-bearer-canary"
                } else {
                    "Bearer github-bearer-canary"
                }
            );
        }
    }
}

impl Drop for IdentityFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn read_identity(
    State(api): State<IdentityApi>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let path = uri.path_and_query().unwrap().as_str().to_owned();
    let mut requests = api.requests.lock().unwrap();
    requests.push(ReadRequest {
        method: method.clone(),
        path: path.clone(),
        authorization: headers
            .get("authorization")
            .and_then(|header| header.to_str().ok())
            .unwrap_or_default()
            .into(),
    });
    if method != Method::GET || requests.len() > 16 {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            Json(json!({"error": "read-only bounded fixture"})),
        )
            .into_response();
    }

    if let Some(status) = api.statuses.lock().unwrap().get(&path).copied() {
        return (
            status,
            [("location", "/must-never-follow")],
            Json(json!({"error": "response-body-secret-canary"})),
        )
            .into_response();
    }

    match api.routes.lock().unwrap().get(&path).cloned() {
        Some(response) => (StatusCode::OK, Json(response)),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unexpected endpoint"})),
        ),
    }
    .into_response()
}

#[derive(Serialize)]
struct SourceDocument<'a> {
    version: u32,
    ingress: &'a IngressSettings,
}

#[tokio::test]
async fn named_repository_adopts_existing_ids_without_modifying_source() {
    let fixture = IdentityFixture::new().await;
    let source = fixture.directory.file(
        "config.toml",
        toml_edit::ser::to_string(&SourceDocument {
            version: 1,
            ingress: &fixture.settings,
        })
        .unwrap(),
    );
    let before = fs::read(&source).unwrap();
    let settings = fixture.settings.clone();
    let authored = CentralConfig::from_toml(std::str::from_utf8(&before).unwrap()).unwrap();
    let adopted = tokio::time::timeout(
        Duration::from_secs(3),
        resolve_identities(authored.ingress.as_ref().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(adopted.controller_id, CONTROLLER);
    assert_eq!(
        adopted.repositories.keys().collect::<Vec<_>>(),
        vec!["4242"]
    );
    let repository = &adopted.repositories["4242"];
    assert_eq!(repository.garm_repository_id, REPOSITORY);
    assert_eq!(repository.template_id, Some(9));
    assert_eq!(
        repository.webhook_secret_file,
        settings.repositories["owner/repo"].webhook_secret_file
    );
    assert_eq!(fixture.settings, settings);
    assert_eq!(fs::read(source).unwrap(), before);
    fixture.assert_only_authenticated_reads();
    let requests = fixture.api.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        [
            "/api/v1/controller",
            "/api/v1/repositories",
            "/api/v1/github/credentials",
            "/repos/Owner/Repo",
            "/repos/Owner/Repo/actions/runs?per_page=1"
        ]
    );
}

#[tokio::test]
async fn numeric_repository_retains_and_verifies_its_existing_ids() {
    let mut fixture = IdentityFixture::new().await;
    let mut repository = fixture.settings.repositories.remove("owner/repo").unwrap();
    repository.garm_repository_id = REPOSITORY.into();
    fixture
        .settings
        .repositories
        .insert("4242".into(), repository);
    fixture.settings.controller_id = CONTROLLER.into();
    assert_eq!(
        resolve_identities(&fixture.settings).await.unwrap(),
        fixture.settings
    );
    fixture.assert_only_authenticated_reads();
}

#[derive(Clone, Copy, Debug)]
enum IdentityMismatch {
    Controller,
    InvalidController,
    Repository,
    InvalidRepository,
    CredentialLink,
    DuplicateCredential,
    AuthType,
    ForgeType,
    Endpoint,
    AgentMode,
    GithubName,
    GithubId,
    DuplicateRepository,
}

#[tokio::test]
async fn conflicting_api_identities_fail_before_any_mutation() {
    for mismatch in [
        IdentityMismatch::Controller,
        IdentityMismatch::InvalidController,
        IdentityMismatch::Repository,
        IdentityMismatch::InvalidRepository,
        IdentityMismatch::CredentialLink,
        IdentityMismatch::DuplicateCredential,
        IdentityMismatch::AuthType,
        IdentityMismatch::ForgeType,
        IdentityMismatch::Endpoint,
        IdentityMismatch::AgentMode,
        IdentityMismatch::GithubName,
        IdentityMismatch::GithubId,
        IdentityMismatch::DuplicateRepository,
    ] {
        let mut fixture = IdentityFixture::new().await;
        match mismatch {
            IdentityMismatch::Controller => {
                fixture.settings.controller_id = OTHER_REPOSITORY.into()
            }
            IdentityMismatch::InvalidController => fixture.change("/api/v1/controller", |value| {
                value["controller_id"] = json!("not-uuid")
            }),
            IdentityMismatch::Repository => {
                fixture
                    .settings
                    .repositories
                    .get_mut("owner/repo")
                    .unwrap()
                    .garm_repository_id = OTHER_REPOSITORY.into()
            }
            IdentityMismatch::InvalidRepository => fixture
                .change("/api/v1/repositories", |value| {
                    value[0]["id"] = json!("not-uuid")
                }),
            IdentityMismatch::CredentialLink => fixture.change("/api/v1/repositories", |value| {
                value[0]["credentials_id"] = json!(88)
            }),
            IdentityMismatch::DuplicateCredential => {
                fixture.change("/api/v1/github/credentials", |value| {
                    let duplicate = value[0].clone();
                    value.as_array_mut().unwrap().push(duplicate);
                })
            }
            IdentityMismatch::AuthType => fixture.change("/api/v1/github/credentials", |value| {
                value[0]["auth-type"] = json!("pat")
            }),
            IdentityMismatch::ForgeType => fixture.change("/api/v1/github/credentials", |value| {
                value[0]["forge_type"] = json!("gitea")
            }),
            IdentityMismatch::Endpoint => fixture.change("/api/v1/github/credentials", |value| {
                value[0]["endpoint"]["api_base_url"] = json!("https://other.example/")
            }),
            IdentityMismatch::AgentMode => fixture.change("/api/v1/repositories", |value| {
                value[0]["agent_mode"] = json!(true)
            }),
            IdentityMismatch::GithubName => fixture.change("/repos/Owner/Repo", |value| {
                value["full_name"] = json!("Different/Repo")
            }),
            IdentityMismatch::GithubId => {
                let mut repository = fixture.settings.repositories.remove("owner/repo").unwrap();
                repository.garm_repository_id = REPOSITORY.into();
                fixture
                    .settings
                    .repositories
                    .insert("9999".into(), repository);
            }
            IdentityMismatch::DuplicateRepository => {
                fixture.change("/api/v1/repositories", |value| {
                    let duplicate = value[0].clone();
                    value.as_array_mut().unwrap().push(duplicate);
                })
            }
        }

        let before = fixture.settings.clone();
        assert!(
            matches!(
                resolve_identities(&fixture.settings).await,
                Err(SetupError::Identity)
            ),
            "{mismatch:?}"
        );
        assert_eq!(fixture.settings, before);
        fixture.assert_only_authenticated_reads();
    }
}

#[tokio::test]
async fn duplicate_named_and_numeric_keys_cannot_alias_one_repository() {
    let mut fixture = IdentityFixture::new().await;
    let mut alias = fixture.settings.repositories["owner/repo"].clone();
    alias.garm_repository_id = REPOSITORY.into();
    fixture.settings.repositories.insert("4242".into(), alias);
    assert!(matches!(
        resolve_identities(&fixture.settings).await,
        Err(SetupError::Identity)
    ));
    fixture.assert_only_authenticated_reads();
}

#[tokio::test]
async fn missing_or_empty_original_webhook_secret_prevents_adoption() {
    for missing in [false, true] {
        let fixture = IdentityFixture::new().await;
        let path = &fixture.settings.repositories["owner/repo"].webhook_secret_file;
        if missing {
            fs::remove_file(path).unwrap();
        } else {
            fs::write(path, "").unwrap();
        }
        assert!(matches!(
            resolve_identities(&fixture.settings).await,
            Err(SetupError::ProtectedConfiguration | SetupError::Configuration)
        ));
        fixture.assert_only_authenticated_reads();
        assert!(
            fixture
                .api
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| !request.path.contains("actions/runs"))
        );
    }
}

#[tokio::test]
async fn unreadable_actions_prevent_adoption() {
    let fixture = IdentityFixture::new().await;
    fixture
        .api
        .routes
        .lock()
        .unwrap()
        .remove("/repos/Owner/Repo/actions/runs?per_page=1");
    assert!(matches!(
        resolve_identities(&fixture.settings).await,
        Err(SetupError::Identity)
    ));
    fixture.assert_only_authenticated_reads();
}

#[tokio::test]
async fn rejected_or_redirecting_apis_do_not_retry_or_expose_response_bodies() {
    for status in [StatusCode::FORBIDDEN, StatusCode::FOUND] {
        let fixture = IdentityFixture::new().await;
        fixture
            .api
            .statuses
            .lock()
            .unwrap()
            .insert("/api/v1/controller".into(), status);
        let error = resolve_identities(&fixture.settings).await.unwrap_err();
        assert!(matches!(error, SetupError::Identity));
        assert!(!error.to_string().contains("response-body-secret-canary"));
        assert_eq!(fixture.api.requests.lock().unwrap().len(), 1);
        fixture.assert_only_authenticated_reads();
    }
}

#[tokio::test]
async fn unsafe_api_origins_are_rejected_before_sending_bearer_tokens() {
    for origin in [
        "http://example.com/",
        "http://user:password@127.0.0.1/",
        "https://api.example/?secret=canary",
        "https://api.example/#fragment",
    ] {
        let mut fixture = IdentityFixture::new().await;
        fixture.settings.github_api_origin = origin.into();
        assert!(matches!(
            resolve_identities(&fixture.settings).await,
            Err(SetupError::Identity)
        ));
        assert!(fixture.api.requests.lock().unwrap().is_empty());
    }
}
