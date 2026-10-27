//! Daemon-owned issuance into the token files reread by the existing API clients.

use crate::config::IngressSettings;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::{Client, Response, Url};
use serde::{Deserialize, Serialize};
use std::{fs::File, path::PathBuf, time::Duration};
use thiserror::Error;

mod protected_files;

use protected_files::{TokenFile, read_secret};

const BODY_LIMIT: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_SECONDS: i64 = 30;
const MINIMUM_TTL: i64 = 30;

/// References to existing authority; secret contents never enter normalized policy.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenRenewalConfig {
    pub github_app_id: u64,
    pub github_installation_id: u64,
    pub github_private_key_file: PathBuf,
    pub garm_credentials_file: PathBuf,
}

/// Fixed messages deliberately exclude URLs, response bodies and credentials.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("invalid token renewal configuration")]
    Configuration,
    #[error("token renewal requires protected regular files and directories")]
    PrivateFile,
    #[error("token renewal output directory is already locked")]
    Locked,
    #[error("GitHub installation token issuance failed")]
    Github,
    #[error("GARM login token issuance failed")]
    Garm,
}

/// Safe operational state. Expiries are Unix seconds; no bearer material is exposed.
#[derive(Clone, Debug)]
pub struct RenewalReport {
    pub github_expires_at: Option<i64>,
    pub garm_expires_at: Option<i64>,
    pub next_refresh_in: Duration,
}

pub struct TokenRenewal {
    client: Client,
    config: TokenRenewalConfig,
    github_url: Url,
    garm_url: Url,
    github: ManagedToken,
    garm: ManagedToken,
    _locks: Vec<File>,
}

struct ManagedToken {
    output: TokenFile,
    expires_at: Option<i64>,
    next_attempt: i64,
}

struct IssuedToken {
    value: String,
    expires_at: i64,
}

#[derive(Serialize)]
struct GithubClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

#[derive(Deserialize)]
struct GithubTokenResponse {
    token: String,
    expires_at: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GarmCredentials {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct GarmLoginResponse {
    token: String,
}

#[derive(Deserialize)]
struct JwtHeader {
    alg: String,
}

#[derive(Deserialize)]
struct JwtClaims {
    exp: i64,
}

impl TokenRenewal {
    /// Validate references, lock outputs and obtain initial authority before ingress starts.
    ///
    /// # Errors
    /// Returns a redacted error for unsafe configuration/files or failed issuance.
    pub async fn connect(
        config: TokenRenewalConfig,
        ingress: &IngressSettings,
    ) -> Result<Self, AuthError> {
        if config.github_app_id == 0 || config.github_installation_id == 0 {
            return Err(AuthError::Configuration);
        }

        let github_url = endpoint(
            &ingress.github_api_origin,
            &format!(
                "app/installations/{}/access_tokens",
                config.github_installation_id
            ),
        )?;
        let garm_url = endpoint(&ingress.garm_api_origin, "api/v1/auth/login")?;
        let paths = [
            config.github_private_key_file.as_path(),
            config.garm_credentials_file.as_path(),
            std::path::Path::new(&ingress.github_token_file),
            std::path::Path::new(&ingress.garm_token_file),
        ];
        for (index, path) in paths.iter().enumerate() {
            if paths
                .iter()
                .skip(index + 1)
                .any(|other| path.starts_with(other) || other.starts_with(path))
            {
                return Err(AuthError::Configuration);
            }
        }

        // Check both inputs before touching existing outputs. Re-read on each issuance,
        // allowing protected key/password rotation without restarting the daemon.
        rsa_key(&config)?;
        credentials(&config)?;
        let github_output = TokenFile::open(paths[2])?;
        let garm_output = TokenFile::open(paths[3])?;
        let mut locks = vec![github_output.lock_directory()?];
        if !github_output.same_directory(&garm_output)? {
            locks.push(garm_output.lock_directory()?);
        }

        let client = Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!(
                "garm-provider-outrider/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|_| AuthError::Configuration)?;
        // An old opaque installation token has no trustworthy expiry after restart.
        github_output.invalidate()?;
        garm_output.invalidate()?;
        let mut manager = Self {
            client,
            config,
            github_url,
            garm_url,
            github: ManagedToken::new(github_output),
            garm: ManagedToken::new(garm_output),
            _locks: locks,
        };
        manager.refresh_due().await?;
        Ok(manager)
    }

    /// Issue only due credentials. Each authority retains its own expiry and retry.
    /// Failed/ambiguous POSTs are never replayed within this call.
    ///
    /// # Errors
    /// Returns a fixed error; use `report().next_refresh_in` for bounded retry timing.
    pub async fn refresh_due(&mut self) -> Result<Duration, AuthError> {
        let now = Utc::now().timestamp();
        let github_due = self.github.prepare(now);
        let garm_due = self.garm.prepare(now);
        let github_budget = self.github.request_budget(self.garm.expires_at);
        let garm_budget = self.garm.request_budget(self.github.expires_at);
        // Both requests and their publication run independently. A slow/failing GARM
        // request cannot postpone GitHub publication or expired-file invalidation.
        let github = async {
            if !github_due? {
                return Ok(());
            }

            let issued = tokio::time::timeout(
                github_budget,
                issue_github(&self.client, &self.github_url, &self.config),
            )
            .await
            .map_err(|_| AuthError::Github)
            .and_then(|result| result);
            self.github.finish(issued)
        };
        let garm = async {
            if !garm_due? {
                return Ok(());
            }

            let issued = tokio::time::timeout(
                garm_budget,
                issue_garm(&self.client, &self.garm_url, &self.config),
            )
            .await
            .map_err(|_| AuthError::Garm)
            .and_then(|result| result);
            self.garm.finish(issued)
        };
        let (github, garm) = tokio::join!(github, garm);
        // A previously cached, non-due authority can expire while the other POST
        // is in flight. Remove expired outputs before returning, even on failure.
        let now = Utc::now().timestamp();
        let github_expiry = self.github.prepare(now);
        let garm_expiry = self.garm.prepare(now);
        github?;
        garm?;
        github_expiry?;
        garm_expiry?;
        Ok(self.report().next_refresh_in)
    }

    #[must_use]
    pub fn report(&self) -> RenewalReport {
        let now = Utc::now().timestamp();
        let next = self.github.next_deadline().min(self.garm.next_deadline());
        RenewalReport {
            github_expires_at: self.github.expires_at,
            garm_expires_at: self.garm.expires_at,
            next_refresh_in: Duration::from_secs(next.saturating_sub(now).max(1) as u64),
        }
    }
}

impl ManagedToken {
    fn new(output: TokenFile) -> Self {
        Self {
            output,
            expires_at: None,
            next_attempt: 0,
        }
    }

    fn prepare(&mut self, now: i64) -> Result<bool, AuthError> {
        if self.expires_at.is_some_and(|expiry| expiry <= now) {
            self.output.invalidate()?;
            self.expires_at = None;
        }

        Ok(now >= self.next_attempt)
    }

    fn next_deadline(&self) -> i64 {
        self.expires_at
            .map_or(self.next_attempt, |expiry| expiry.min(self.next_attempt))
    }

    fn request_budget(&self, other_expiry: Option<i64>) -> Duration {
        let expires_at = self.expires_at.into_iter().chain(other_expiry).min();
        expires_at.map_or(REQUEST_TIMEOUT, |expiry| {
            Duration::from_secs(expiry.saturating_sub(Utc::now().timestamp()).max(1) as u64)
                .min(REQUEST_TIMEOUT)
        })
    }

    fn finish(&mut self, issued: Result<IssuedToken, AuthError>) -> Result<(), AuthError> {
        let now = Utc::now().timestamp();
        self.next_attempt = now.saturating_add(RETRY_SECONDS);
        match issued {
            Ok(token) => {
                // Disk publication can fail independently of issuance; retain the old
                // expiry until the replacement has actually been durably published.
                let published = self.output.publish(token.value.as_bytes());
                if published.is_ok() {
                    self.expires_at = Some(token.expires_at);
                    let ttl = token.expires_at.saturating_sub(now);
                    let margin = (ttl / 5).clamp(1, 300);
                    self.next_attempt = token.expires_at.saturating_sub(margin);
                }

                self.prepare(now)?;
                published
            }
            Err(error) => {
                self.prepare(now)?;
                Err(error)
            }
        }
    }
}

fn endpoint(origin: &str, path: &str) -> Result<Url, AuthError> {
    let authority = origin
        .split_once("://")
        .map(|(_, tail)| tail.split(['/', '?', '#']).next().unwrap_or_default());
    if authority.is_some_and(|value| value.contains('@')) {
        return Err(AuthError::Configuration);
    }

    let mut origin = Url::parse(origin).map_err(|_| AuthError::Configuration)?;
    let loopback = origin.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if !(origin.scheme() == "https" || origin.scheme() == "http" && loopback)
        || origin.host().is_none()
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return Err(AuthError::Configuration);
    }

    if !origin.path().ends_with('/') {
        let path = format!("{}/", origin.path());
        origin.set_path(&path);
    }

    origin.join(path).map_err(|_| AuthError::Configuration)
}

fn rsa_key(config: &TokenRenewalConfig) -> Result<EncodingKey, AuthError> {
    EncodingKey::from_rsa_pem(&read_secret(&config.github_private_key_file)?)
        .map_err(|_| AuthError::Github)
}

fn credentials(config: &TokenRenewalConfig) -> Result<GarmCredentials, AuthError> {
    let bytes = read_secret(&config.garm_credentials_file)?;
    let credentials: GarmCredentials =
        serde_json::from_slice(&bytes).map_err(|_| AuthError::Garm)?;
    if credentials.username.trim().is_empty() || credentials.password.is_empty() {
        return Err(AuthError::Garm);
    }

    Ok(credentials)
}

async fn issue_github(
    client: &Client,
    url: &Url,
    config: &TokenRenewalConfig,
) -> Result<IssuedToken, AuthError> {
    let now = Utc::now().timestamp();
    let claims = GithubClaims {
        iat: now - 60,
        exp: now + 540,
        iss: config.github_app_id.to_string(),
    };
    let jwt = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &rsa_key(config)?)
        .map_err(|_| AuthError::Github)?;
    let response = client
        .post(url.clone())
        .bearer_auth(jwt)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|_| AuthError::Github)?;
    if response.status() != reqwest::StatusCode::CREATED {
        return Err(AuthError::Github);
    }

    let body = read_body(response).await.map_err(|_| AuthError::Github)?;
    let token: GithubTokenResponse =
        serde_json::from_slice(&body).map_err(|_| AuthError::Github)?;
    let expires_at = DateTime::parse_from_rfc3339(&token.expires_at)
        .map_err(|_| AuthError::Github)?
        .timestamp();
    validate_token(token.token, expires_at).ok_or(AuthError::Github)
}

async fn issue_garm(
    client: &Client,
    url: &Url,
    config: &TokenRenewalConfig,
) -> Result<IssuedToken, AuthError> {
    let credentials = credentials(config)?;
    let response = client
        .post(url.clone())
        .json(&credentials)
        .send()
        .await
        .map_err(|_| AuthError::Garm)?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(AuthError::Garm);
    }

    let body = read_body(response).await.map_err(|_| AuthError::Garm)?;
    let response: GarmLoginResponse = serde_json::from_slice(&body).map_err(|_| AuthError::Garm)?;
    let mut segments = response.token.split('.');
    let header = segments.next().ok_or(AuthError::Garm)?;
    let payload = segments.next().ok_or(AuthError::Garm)?;
    let signature = segments.next().ok_or(AuthError::Garm)?;
    if header.is_empty() || signature.is_empty() || segments.next().is_some() {
        return Err(AuthError::Garm);
    }

    let header: JwtHeader = URL_SAFE_NO_PAD
        .decode(header)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or(AuthError::Garm)?;
    if header.alg.is_empty()
        || header.alg.eq_ignore_ascii_case("none")
        || !URL_SAFE_NO_PAD
            .decode(signature)
            .is_ok_and(|bytes| !bytes.is_empty())
    {
        return Err(AuthError::Garm);
    }

    // Consistency check of the authenticated loopback/HTTPS login response only.
    // GARM's JWT signing key is deliberately neither read nor used for verification.
    let claims: JwtClaims = URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|payload| serde_json::from_slice(&payload).ok())
        .ok_or(AuthError::Garm)?;
    validate_token(response.token, claims.exp).ok_or(AuthError::Garm)
}

fn validate_token(value: String, expires_at: i64) -> Option<IssuedToken> {
    if DateTime::from_timestamp(expires_at, 0).is_none()
        || value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
        || expires_at.saturating_sub(Utc::now().timestamp()) < MINIMUM_TTL
    {
        return None;
    }

    Some(IssuedToken { value, expires_at })
}

async fn read_body(mut response: Response) -> Result<Vec<u8>, ()> {
    if response
        .content_length()
        .is_some_and(|size| size > BODY_LIMIT as u64)
    {
        return Err(());
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if chunk.len() > BODY_LIMIT.saturating_sub(body.len()) {
            return Err(());
        }

        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

#[cfg(test)]
mod tests;
