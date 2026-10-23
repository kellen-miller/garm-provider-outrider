use std::{
    fs,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::time::timeout;

use crate::{
    config::IngressSettings,
    journal::{Journal, PoolSnapshot},
};

#[derive(Debug, Error)]
pub enum PoolError {
    #[error("invalid GARM configuration")]
    Configuration,
    #[error("administrator allocation admission is closed")]
    AdmissionClosed,
    #[error("GARM request failed")]
    Request,
    #[error("GARM pool state is ambiguous")]
    Ambiguous,
    #[error("exact GARM repository pool was not found")]
    NotFound,
    #[error("repository pool limit reached")]
    Limit,
    #[error("routing token conflicts with an existing GARM pool")]
    Conflict,
    #[error("unsupported operating system for GARM pool creation")]
    UnsupportedOs,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GarmPool {
    pub id: String,
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub tags: Vec<GarmTag>,
    #[serde(default)]
    pub extra_specs: Value,
    #[serde(default)]
    pub max_runners: u32,
    #[serde(default)]
    pub min_idle_runners: u32,
    #[serde(default)]
    pub os_type: String,
    #[serde(default)]
    pub os_arch: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub flavor: String,
    #[serde(default)]
    pub runner_bootstrap_timeout: u32,
    #[serde(default)]
    pub runner_prefix: String,
    #[serde(default)]
    pub template_id: u32,
    #[serde(default)]
    pub repo_id: String,
    #[serde(default)]
    pub instances: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GarmTag {
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CreatePool {
    pub provider_name: String,
    pub max_runners: u32,
    pub min_idle_runners: u32,
    pub tags: Vec<String>,
    pub flavor: String,
    pub image: String,
    pub runner_prefix: String,
    pub os_type: String,
    pub os_arch: String,
    pub enabled: bool,
    pub runner_bootstrap_timeout: u32,
    pub extra_specs: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_id: Option<u32>,
}

/// Immutable inputs used to reconcile one repository-scoped GARM pool.
pub struct RepositoryPoolRequest<'a> {
    pub snapshot: &'a PoolSnapshot,
    pub admission: Option<&'a crate::config::AdmissionSettings>,
}

#[derive(Clone)]
pub struct GarmClient {
    client: Client,
    origin: Url,
    token_file: String,
    timeout: Duration,
}

impl GarmClient {
    pub(crate) fn verify_origin(&self, expected: &str) -> Result<(), PoolError> {
        let expected = Url::parse(expected).map_err(|_| PoolError::Configuration)?;
        if self.origin != expected {
            return Err(PoolError::Configuration);
        }

        Ok(())
    }

    /// Build the authenticated transport using only the configured GARM origin and token file.
    ///
    /// # Errors
    /// Returns `PoolError::Configuration` when the origin or credential file is invalid.
    pub fn from_settings(
        settings: &IngressSettings,
        request_timeout: Duration,
    ) -> Result<Self, PoolError> {
        let origin = Url::parse(&settings.garm_api_origin).map_err(|_| PoolError::Configuration)?;
        if !matches!(origin.scheme(), "http" | "https")
            || origin.host_str().is_none()
            || origin.username() != ""
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || !origin.path().ends_with('/')
            || request_timeout.is_zero()
        {
            return Err(PoolError::Configuration);
        }
        let token =
            fs::read_to_string(&settings.garm_token_file).map_err(|_| PoolError::Configuration)?;
        if token.trim().is_empty() {
            return Err(PoolError::Configuration);
        }
        let client = Client::builder()
            .timeout(request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| PoolError::Configuration)?;
        Ok(Self {
            client,
            origin,
            token_file: settings.garm_token_file.clone(),
            timeout: request_timeout,
        })
    }

    /// Reconcile an exact repository pool, persisting its snapshot before enabling it.
    ///
    /// # Errors
    /// Returns `PoolError` when repository mapping, pool reconciliation, or persistence fails.
    pub async fn ensure_repository_pool(
        &self,
        journal: &Journal,
        request: RepositoryPoolRequest<'_>,
    ) -> Result<GarmPool, PoolError> {
        let snapshot = request.snapshot;
        if !crate::admission::allocation_is_open(request.admission) {
            return Err(PoolError::AdmissionClosed);
        }

        if !matches!(
            snapshot.state.as_str(),
            "prepared" | "creating" | "disabled" | "enabled"
        ) || snapshot.expires_at
            <= SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        {
            return Err(PoolError::Limit);
        }
        journal
            .save_pool_snapshot(snapshot)
            .map_err(|_| PoolError::Request)?;
        let durable = journal
            .pool_snapshot(snapshot.repository_id, &snapshot.request_token)
            .map_err(|_| PoolError::Request)?
            .ok_or(PoolError::Request)?;
        if !matches!(
            durable.state.as_str(),
            "prepared" | "creating" | "disabled" | "enabled"
        ) || durable.expires_at <= unix_now()
        {
            return Err(PoolError::Limit);
        }
        let document: crate::github::SnapshotDocument =
            serde_json::from_str(&snapshot.config_json).map_err(|_| PoolError::Configuration)?;
        let settings = document
            .central
            .ingress
            .as_ref()
            .ok_or(PoolError::Configuration)?;
        let repository = settings
            .repositories
            .get(&document.base_repository_id.to_string())
            .ok_or(PoolError::Configuration)?;
        let repository_id = repository.garm_repository_id.as_str();
        self.verify_origin(&settings.garm_api_origin)?;
        let profile = &document.resolved_profile;
        let template_id = repository.template_id;
        if matches!(
            profile.os.to_ascii_lowercase().as_str(),
            "macos" | "osx" | "darwin"
        ) {
            return Err(PoolError::UnsupportedOs);
        }
        self.verify_repository_mapping(repository_id, &snapshot.config_json)
            .await?;
        let token = snapshot.request_token.as_str();
        let wanted_extra = json!({"snapshot_id": snapshot.id});
        let timeout_minutes = required_timeout_minutes(settings)?;
        let create = CreatePool {
            provider_name: settings.provider_name.clone(),
            max_runners: settings.max_runners_per_pool,
            min_idle_runners: 0,
            tags: vec![token.to_owned()],
            flavor: settings.pool_flavor.clone(),
            image: settings.pool_image.clone(),
            runner_prefix: settings.runner_prefix.clone(),
            os_type: profile.os.clone(),
            os_arch: profile.architecture.clone(),
            enabled: false,
            runner_bootstrap_timeout: timeout_minutes,
            extra_specs: wanted_extra,
            template_id,
        };
        let pools = self.request_pools(repository_id, token).await?;
        let conflicting = pools.iter().any(|pool| {
            pool.tags.iter().any(|tag| tag.name == token)
                && !pool_matches(pool, &create, repository_id)
        });
        if conflicting {
            return Err(PoolError::Conflict);
        }
        let matches = pools
            .into_iter()
            .filter(|pool| pool_matches(pool, &create, repository_id))
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(PoolError::Ambiguous);
        }
        let existing = matches.into_iter().next();
        let pool = match existing {
            Some(pool) => pool,
            None => {
                if durable.state != "prepared" {
                    return Err(PoolError::Ambiguous);
                }
                if !crate::admission::allocation_is_open(request.admission) {
                    return Err(PoolError::AdmissionClosed);
                }

                journal
                    .set_pool_snapshot_state(&snapshot.id, "creating", None)
                    .map_err(|_| PoolError::Request)?;
                let _create_result = self.post_pool(repository_id, &create).await;
                let reconciled = self
                    .request_pools(repository_id, token)
                    .await?
                    .into_iter()
                    .filter(|pool| pool_matches(pool, &create, repository_id))
                    .collect::<Vec<_>>();
                if reconciled.len() != 1 {
                    return Err(if reconciled.len() > 1 {
                        PoolError::Ambiguous
                    } else {
                        PoolError::Request
                    });
                }
                reconciled[0].clone()
            }
        };

        journal
            .set_pool_snapshot_state(&snapshot.id, "disabled", Some(&pool.id))
            .map_err(|_| PoolError::Request)?;
        if !self.admission_is_open(journal, &durable, &document)? {
            journal
                .set_pool_snapshot_state(&snapshot.id, "draining", Some(&pool.id))
                .map_err(|_| PoolError::Request)?;
            self.disable_repository_pool(repository_id, &pool.id)
                .await?;
            return Err(PoolError::Limit);
        }
        if !crate::admission::allocation_is_open(request.admission) {
            return Err(PoolError::AdmissionClosed);
        }

        if !pool.enabled {
            let path = format!(
                "api/v1/repositories/{repository_id}/pools/{}",
                path_component(&pool.id)
            );
            self.request_json(reqwest::Method::PUT, &path, Some(&json!({"enabled": true})))
                .await?;
        }
        if !self.admission_is_open(journal, &durable, &document)? {
            journal
                .set_pool_snapshot_state(&snapshot.id, "draining", Some(&pool.id))
                .map_err(|_| PoolError::Request)?;
            self.disable_repository_pool(repository_id, &pool.id)
                .await?;
            return Err(PoolError::Limit);
        }
        if !crate::admission::allocation_is_open(request.admission) {
            self.disable_repository_pool(repository_id, &pool.id)
                .await?;
            return Err(PoolError::AdmissionClosed);
        }

        journal
            .set_pool_snapshot_state(&snapshot.id, "enabled", Some(&pool.id))
            .map_err(|_| PoolError::Request)?;
        Ok(GarmPool {
            enabled: true,
            ..pool
        })
    }

    fn admission_is_open(
        &self,
        journal: &Journal,
        snapshot: &crate::journal::PoolSnapshot,
        document: &crate::github::SnapshotDocument,
    ) -> Result<bool, PoolError> {
        let current = journal
            .pool_snapshot(snapshot.repository_id, &snapshot.request_token)
            .map_err(|_| PoolError::Request)?
            .ok_or(PoolError::Request)?;
        let state_open = matches!(current.state.as_str(), "disabled" | "enabled");
        let terminal = journal
            .run_is_terminal(
                document.base_repository_id,
                document.run_id,
                document.run_attempt,
            )
            .map_err(|_| PoolError::Request)?;
        Ok(state_open && !terminal && current.expires_at > unix_now())
    }

    /// Read complete details for a request's candidate repository pools.
    ///
    /// # Errors
    /// Returns `PoolError` when GARM is unavailable or returns an invalid pool list.
    async fn request_pools(
        &self,
        repository_id: &str,
        request_token: &str,
    ) -> Result<Vec<GarmPool>, PoolError> {
        let path = format!(
            "api/v1/repositories/{}/pools",
            path_component(repository_id)
        );
        let value: Value = self.request_json(reqwest::Method::GET, &path, None).await?;
        let pools_value = value.get("pools").cloned().unwrap_or(value);
        let summaries: Vec<GarmPool> =
            serde_json::from_value(pools_value).map_err(|_| PoolError::Request)?;
        let mut pools = Vec::with_capacity(summaries.len());
        for summary in summaries {
            // Empty/missing summary tags require detail before declaring absence.
            if !summary.tags.is_empty() && !summary.tags.iter().any(|tag| tag.name == request_token)
            {
                continue;
            }

            let path = format!(
                "api/v1/repositories/{}/pools/{}",
                path_component(repository_id),
                path_component(&summary.id)
            );
            let Some(detail) = self
                .request_json_optional(reqwest::Method::GET, &path, None)
                .await?
            else {
                continue;
            };

            let pool: GarmPool = serde_json::from_value(detail).map_err(|_| PoolError::Request)?;
            if pool.id != summary.id || !pool.repo_id.eq_ignore_ascii_case(repository_id) {
                return Err(PoolError::Ambiguous);
            }

            pools.push(pool);
        }

        Ok(pools)
    }

    /// Delete a pool only when GARM reports no instances and no operations remain.
    ///
    /// # Errors
    /// Returns `PoolError::Limit` while work remains, or `PoolError::Request` on GARM errors.
    pub async fn delete_drained_pool(
        &self,
        repository_id: &str,
        pool_id: &str,
        outstanding_operations: usize,
    ) -> Result<(), PoolError> {
        let detail_path = format!(
            "api/v1/repositories/{}/pools/{}",
            path_component(repository_id),
            path_component(pool_id)
        );
        let pool: Value = self
            .request_json_optional(reqwest::Method::GET, &detail_path, None)
            .await?
            .ok_or(PoolError::NotFound)?;
        if pool.get("enabled").and_then(Value::as_bool) == Some(true) {
            self.request_json(
                reqwest::Method::PUT,
                &detail_path,
                Some(&json!({"enabled": false})),
            )
            .await?;
        }
        if outstanding_operations != 0 {
            return Err(PoolError::Limit);
        }
        let instances_path = format!("api/v1/pools/{}/instances", path_component(pool_id));
        let instances: Value = self
            .request_json(reqwest::Method::GET, &instances_path, None)
            .await?;
        if !instances.as_array().is_some_and(Vec::is_empty) {
            return Err(PoolError::Limit);
        }
        self.request_json(reqwest::Method::DELETE, &detail_path, None)
            .await?;
        Ok(())
    }

    /// Disable one pool immediately when its admission became terminal or expired in flight.
    ///
    /// # Errors
    /// Returns `PoolError` when the pool state cannot be read or disabled.
    pub async fn disable_repository_pool(
        &self,
        repository_id: &str,
        pool_id: &str,
    ) -> Result<(), PoolError> {
        let path = format!(
            "api/v1/repositories/{}/pools/{}",
            path_component(repository_id),
            path_component(pool_id)
        );
        let pool: Value = self.request_json(reqwest::Method::GET, &path, None).await?;
        if pool.get("enabled").and_then(Value::as_bool) == Some(true) {
            self.request_json(
                reqwest::Method::PUT,
                &path,
                Some(&json!({"enabled": false})),
            )
            .await?;
        }
        Ok(())
    }

    /// Disable a request pool even when a prior create response was lost before its ID was journaled.
    ///
    /// # Errors
    /// Returns `PoolError` when the snapshot mapping is invalid or matching pools are ambiguous.
    pub async fn disable_request_pool(
        &self,
        snapshot: &PoolSnapshot,
    ) -> Result<Option<String>, PoolError> {
        let document: crate::github::SnapshotDocument =
            serde_json::from_str(&snapshot.config_json).map_err(|_| PoolError::Configuration)?;
        let settings = document
            .central
            .ingress
            .as_ref()
            .ok_or(PoolError::Configuration)?;
        self.verify_origin(&settings.garm_api_origin)?;
        let repository = settings
            .repositories
            .get(&document.base_repository_id.to_string())
            .ok_or(PoolError::Configuration)?;
        self.verify_repository_mapping(&repository.garm_repository_id, &snapshot.config_json)
            .await?;
        if let Some(pool_id) = snapshot.garm_pool_id.as_deref() {
            let path = format!(
                "api/v1/repositories/{}/pools/{}",
                path_component(&repository.garm_repository_id),
                path_component(pool_id)
            );
            let pool = self
                .request_json_optional(reqwest::Method::GET, &path, None)
                .await?
                .ok_or(PoolError::NotFound)?;
            let returned_id = pool.get("id").and_then(Value::as_str);
            let matches_snapshot = returned_id == Some(pool_id)
                && pool
                    .get("tags")
                    .and_then(Value::as_array)
                    .is_some_and(|tags| {
                        tags.iter().any(|tag| {
                            tag.get("name").and_then(Value::as_str)
                                == Some(snapshot.request_token.as_str())
                        })
                    })
                && pool
                    .get("extra_specs")
                    .and_then(|extra| extra.get("snapshot_id"))
                    .and_then(Value::as_str)
                    == Some(snapshot.id.as_str());
            if !matches_snapshot {
                return Err(PoolError::Ambiguous);
            }
            if pool.get("enabled").and_then(Value::as_bool) == Some(true) {
                self.request_json(
                    reqwest::Method::PUT,
                    &path,
                    Some(&json!({"enabled": false})),
                )
                .await?;
            }
            return Ok(Some(pool_id.to_owned()));
        }
        let matching = self
            .request_pools(&repository.garm_repository_id, &snapshot.request_token)
            .await?
            .into_iter()
            .filter(|pool| {
                pool.tags
                    .iter()
                    .any(|tag| tag.name == snapshot.request_token)
                    && pool.extra_specs.get("snapshot_id").and_then(Value::as_str)
                        == Some(snapshot.id.as_str())
            })
            .collect::<Vec<_>>();
        if matching.len() > 1 {
            return Err(PoolError::Ambiguous);
        }
        let Some(pool) = matching.into_iter().next() else {
            return Ok(None);
        };
        self.disable_repository_pool(&repository.garm_repository_id, &pool.id)
            .await?;
        Ok(Some(pool.id))
    }

    /// Forward original signed bytes to GARM's repository-scoped webhook handler.
    /// Header names must be lowercase, as persisted by authenticated intake.
    ///
    /// # Errors
    /// Returns `PoolError` when the configured webhook route cannot accept the event.
    pub async fn forward_webhook(
        &self,
        controller_id: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(), PoolError> {
        let path = format!("webhooks/{}", path_component(controller_id));
        let url = self
            .origin
            .join(&path)
            .map_err(|_| PoolError::Configuration)?;
        if url.origin() != self.origin.origin() || !url.path().starts_with(self.origin.path()) {
            return Err(PoolError::Configuration);
        }
        // App deliveries say "integration"; these pools always register at repository scope.
        let mut request = self
            .client
            .post(url)
            .header("x-github-hook-installation-target-type", "repository")
            .body(body.to_vec());
        for (name, value) in headers {
            if matches!(
                name.as_str(),
                "x-github-event" | "x-github-delivery" | "x-hub-signature-256"
            ) {
                request = request.header(name, value);
            }
        }
        let response = timeout(self.timeout, request.send())
            .await
            .map_err(|_| PoolError::Request)?
            .map_err(|_| PoolError::Request)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(PoolError::Request)
        }
    }

    async fn post_pool(&self, repository_id: &str, pool: &CreatePool) -> Result<(), PoolError> {
        let path = format!(
            "api/v1/repositories/{}/pools",
            path_component(repository_id)
        );
        let body = serde_json::to_value(pool).map_err(|_| PoolError::Request)?;
        self.request_json(reqwest::Method::POST, &path, Some(&body))
            .await
            .map(|_| ())
    }

    async fn request_json(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, PoolError> {
        self.request_json_optional(method, path, body)
            .await?
            .ok_or(PoolError::Request)
    }

    async fn request_json_optional(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Option<Value>, PoolError> {
        let url = self
            .origin
            .join(path)
            .map_err(|_| PoolError::Configuration)?;
        if url.origin() != self.origin.origin() || !url.path().starts_with(self.origin.path()) {
            return Err(PoolError::Configuration);
        }
        let token = timeout(self.timeout, tokio::fs::read_to_string(&self.token_file))
            .await
            .map_err(|_| PoolError::Request)?
            .map_err(|_| PoolError::Configuration)?;
        if token.trim().is_empty() {
            return Err(PoolError::Configuration);
        }
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(token.trim())
            .header("Accept", "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = timeout(self.timeout, request.send())
            .await
            .map_err(|_| PoolError::Request)?
            .map_err(|_| PoolError::Request)?;
        if response.status().is_redirection() {
            return Err(PoolError::Request);
        }
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(Some(Value::Null));
        }
        response
            .error_for_status()
            .map_err(|_| PoolError::Request)?
            .json()
            .await
            .map(Some)
            .map_err(|_| PoolError::Request)
    }

    async fn verify_repository_mapping(
        &self,
        repository_id: &str,
        snapshot_json: &str,
    ) -> Result<(), PoolError> {
        let document: crate::github::SnapshotDocument =
            serde_json::from_str(snapshot_json).map_err(|_| PoolError::Configuration)?;
        let path = format!("api/v1/repositories/{}", path_component(repository_id));
        let repository: Value = self.request_json(reqwest::Method::GET, &path, None).await?;
        let returned_id = repository
            .get("id")
            .and_then(Value::as_str)
            .ok_or(PoolError::Configuration)?;
        let name = repository
            .get("name")
            .and_then(Value::as_str)
            .ok_or(PoolError::Configuration)?;
        let owner = repository
            .get("owner")
            .and_then(|value| {
                value
                    .as_str()
                    .or_else(|| value.get("login").and_then(Value::as_str))
            })
            .ok_or(PoolError::Configuration)?;
        let expected = &document.base_repository;
        if !returned_id.eq_ignore_ascii_case(repository_id)
            || !name.eq_ignore_ascii_case(&expected.name)
            || !owner.eq_ignore_ascii_case(&expected.owner.login)
        {
            return Err(PoolError::Configuration);
        }
        if let Some(full_name) = repository.get("full_name").and_then(Value::as_str)
            && !full_name.eq_ignore_ascii_case(&expected.full_name)
        {
            return Err(PoolError::Configuration);
        }
        Ok(())
    }
}

fn required_timeout_minutes(settings: &IngressSettings) -> Result<u32, PoolError> {
    let allocation =
        u32::try_from(settings.allocation_timeout_seconds).map_err(|_| PoolError::Configuration)?;
    let margin =
        u32::try_from(settings.timeout_margin_seconds).map_err(|_| PoolError::Configuration)?;
    let seconds = settings
        .bootstrap_timeout_minutes
        .checked_mul(60)
        .and_then(|value| value.checked_add(allocation))
        .and_then(|value| value.checked_add(margin))
        .ok_or(PoolError::Configuration)?;
    let minutes = seconds.div_ceil(60).max(1);
    Ok(minutes)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn path_component(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn pool_matches(pool: &GarmPool, expected: &CreatePool, repository_id: &str) -> bool {
    pool.repo_id == repository_id
        && pool.provider_name == expected.provider_name
        && pool.tags.len() == 1
        && pool.tags[0].name == expected.tags[0]
        && pool.extra_specs == expected.extra_specs
        && pool.max_runners == expected.max_runners
        && pool.min_idle_runners == 0
        && pool.os_type == expected.os_type
        && pool.os_arch == expected.os_arch
        && pool.image == expected.image
        && pool.flavor == expected.flavor
        && pool.runner_bootstrap_timeout == expected.runner_bootstrap_timeout
        && pool.runner_prefix == expected.runner_prefix
        && expected
            .template_id
            .is_none_or(|template_id| pool.template_id == template_id)
}
