//! Adoption verifies visible identities; it never creates or replaces GARM entities.

use super::{SetupError, configuration::ConfigurationFile};
use crate::config::IngressSettings;
use reqwest::{Client, Url};
use serde::{Deserialize, de::DeserializeOwned};
use std::{collections::BTreeMap, path::Path, time::Duration};

#[derive(Deserialize)]
struct Controller {
    controller_id: String,
}

#[derive(Deserialize)]
struct Repository {
    id: String,
    owner: String,
    name: String,
    credentials_id: u64,
    agent_mode: bool,
}

#[derive(Deserialize)]
struct Credential {
    id: u64,
    #[serde(rename = "auth-type")]
    auth_type: String,
    forge_type: String,
    endpoint: Endpoint,
}

#[derive(Deserialize)]
struct Endpoint {
    #[serde(default)]
    api_base_url: String,
}

#[derive(Deserialize)]
struct GithubRepository {
    id: u64,
    full_name: String,
}

struct IdentityReader {
    client: Client,
    github: Url,
    garm: Url,
    github_token: String,
    garm_token: String,
}

pub(crate) async fn resolve_identities(
    ingress: &IngressSettings,
) -> Result<IngressSettings, SetupError> {
    let reader = IdentityReader::new(ingress)?;
    let controller: Controller = reader.get(false, "api/v1/controller").await?;
    if !crate::github::valid_uuid(&controller.controller_id)
        || (!ingress.controller_id.is_empty() && ingress.controller_id != controller.controller_id)
    {
        return Err(SetupError::Identity);
    }

    let repositories: Option<Vec<Repository>> = reader.get(false, "api/v1/repositories").await?;
    let credentials: Option<Vec<Credential>> =
        reader.get(false, "api/v1/github/credentials").await?;
    let repositories = repositories.unwrap_or_default();
    let credentials = credentials.unwrap_or_default();
    let mut resolved = BTreeMap::new();
    if ingress.repositories.is_empty() {
        return Err(SetupError::Configuration);
    }

    for (key, configured) in &ingress.repositories {
        let matches = repositories
            .iter()
            .filter(|repository| {
                if key.contains('/') {
                    format!("{}/{}", repository.owner, repository.name).eq_ignore_ascii_case(key)
                } else {
                    repository.id == configured.garm_repository_id
                }
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(SetupError::Identity);
        }

        let repository = matches[0];
        if repository.agent_mode
            || !crate::github::valid_uuid(&repository.id)
            || (!configured.garm_repository_id.is_empty()
                && configured.garm_repository_id != repository.id)
        {
            return Err(SetupError::Identity);
        }

        let linked = credentials
            .iter()
            .filter(|credential| credential.id == repository.credentials_id)
            .collect::<Vec<_>>();
        if linked.len() != 1 || linked[0].auth_type != "app" || linked[0].forge_type != "github" {
            return Err(SetupError::Identity);
        }

        let endpoint = if linked[0].endpoint.api_base_url.is_empty() {
            "https://api.github.com/"
        } else {
            linked[0].endpoint.api_base_url.as_str()
        };
        if api_origin(endpoint)? != reader.github {
            return Err(SetupError::Identity);
        }

        let github: GithubRepository = reader
            .get(
                true,
                &format!(
                    "repos/{}/{}",
                    urlencoding::encode(&repository.owner),
                    urlencoding::encode(&repository.name)
                ),
            )
            .await?;
        let expected_name = format!("{}/{}", repository.owner, repository.name);
        if github.id == 0
            || !github.full_name.eq_ignore_ascii_case(&expected_name)
            || (!key.contains('/') && key.parse::<u64>().ok() != Some(github.id))
        {
            return Err(SetupError::Identity);
        }

        // A readable original secret is necessary. The APIs deliberately hide its value;
        // only a subsequent signed delivery can prove GARM/Outrider secret agreement.
        let secret = ConfigurationFile::open(Path::new(&configured.webhook_secret_file))?;
        if secret.bytes().is_empty() {
            return Err(SetupError::Configuration);
        }

        // This GET proves Actions read access without creating runs, checks or runners.
        let _: serde_json::Value = reader
            .get(
                true,
                &format!(
                    "repos/{}/{}/actions/runs?per_page=1",
                    urlencoding::encode(&repository.owner),
                    urlencoding::encode(&repository.name)
                ),
            )
            .await?;
        let mut settings = configured.clone();
        settings.garm_repository_id.clone_from(&repository.id);
        if resolved.insert(github.id.to_string(), settings).is_some() {
            return Err(SetupError::Identity);
        }
    }

    let mut settings = ingress.clone();
    settings.controller_id = controller.controller_id;
    settings.repositories = resolved;
    Ok(settings)
}

impl IdentityReader {
    fn new(ingress: &IngressSettings) -> Result<Self, SetupError> {
        let github_token = ConfigurationFile::open(Path::new(&ingress.github_token_file))?
            .text()?
            .trim()
            .to_owned();
        let garm_token = ConfigurationFile::open(Path::new(&ingress.garm_token_file))?
            .text()?
            .trim()
            .to_owned();
        if github_token.is_empty() || garm_token.is_empty() {
            return Err(SetupError::Identity);
        }

        let client = Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("outrider/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| SetupError::Identity)?;
        Ok(Self {
            client,
            github: api_origin(&ingress.github_api_origin)?,
            garm: api_origin(&ingress.garm_api_origin)?,
            github_token,
            garm_token,
        })
    }

    async fn get<T: DeserializeOwned>(&self, github: bool, path: &str) -> Result<T, SetupError> {
        let (origin, token) = if github {
            (&self.github, &self.github_token)
        } else {
            (&self.garm, &self.garm_token)
        };
        let url = origin.join(path).map_err(|_| SetupError::Identity)?;
        let mut response = self
            .client
            .get(url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|_| SetupError::Identity)?;
        if !response.status().is_success() {
            return Err(SetupError::Identity);
        }

        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| SetupError::Identity)? {
            if bytes.len().saturating_add(chunk.len()) > 4 * 1024 * 1024 {
                return Err(SetupError::Identity);
            }

            bytes.extend_from_slice(&chunk);
        }

        serde_json::from_slice(&bytes).map_err(|_| SetupError::Identity)
    }
}

fn api_origin(input: &str) -> Result<Url, SetupError> {
    let mut url = Url::parse(input).map_err(|_| SetupError::Identity)?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(SetupError::Identity);
    }

    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }

    Ok(url)
}
