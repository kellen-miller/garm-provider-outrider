use std::{
    collections::{BTreeMap, HashSet},
    fs,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::time::timeout;

use crate::{
    config::{ConfigPatch, IngressSettings, RunnerProfile},
    journal::{
        JobObservationRecord, JobRerunReservation, Journal, OutboxMessage, PoolSnapshot,
        WebhookDelivery,
    },
    label::RoutingRequest,
    pools::{GarmClient, PoolError, RepositoryPoolRequest},
};

type HookMac = Hmac<Sha256>;

#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("invalid GitHub API configuration")]
    Configuration,
    #[error("webhook signature or delivery is invalid")]
    Signature,
    #[error("GitHub provenance is unavailable or inconsistent")]
    Provenance,
    #[error("GitHub request failed")]
    Request,
    #[error("configuration file is missing or invalid")]
    Contents,
}

#[derive(Debug, Error)]
pub enum IngressError {
    #[error("invalid ingress configuration")]
    Configuration,
    #[error("durable ingress journal failed")]
    Journal,
    #[error("webhook is invalid")]
    Webhook,
    #[error("GitHub dependency is unavailable")]
    GitHub,
    #[error("GARM dependency is unavailable")]
    Garm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RerunSubmission {
    Acknowledged,
    Denied,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForwardHeaders(pub BTreeMap<String, String>);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotDocument {
    pub base_repository: RepositoryRef,
    pub base_repository_id: u64,
    pub source_repository_id: u64,
    pub run_id: u64,
    pub run_attempt: u32,
    pub job_id: u64,
    pub commit_sha: String,
    pub central: ConfigPatch,
    pub repository: ConfigPatch,
    pub resolved_profile: RunnerProfile,
    pub image_catalogs: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckOutbox {
    pub repository: RepositoryRef,
    pub check: Value,
}

#[derive(Clone)]
pub struct IngressService {
    config: ConfigPatch,
    settings: IngressSettings,
    journal: Arc<Journal>,
    github: GitHubClient,
    garm: GarmClient,
    processor_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RepositoryRef {
    pub id: u64,
    pub name: String,
    pub full_name: String,
    pub owner: OwnerRef,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OwnerRef {
    pub login: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowJobEvent {
    pub action: Option<String>,
    pub repository: RepositoryRef,
    pub workflow_job: WorkflowJobRef,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowJobRef {
    pub id: u64,
    pub run_id: u64,
    pub run_attempt: Option<u32>,
    pub head_sha: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub runner_name: Option<String>,
    pub runner_id: Option<u64>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub run_attempt: u32,
    pub event: String,
    pub head_sha: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    pub repository: RepositoryRef,
    pub head_repository: Option<RepositoryRef>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct WorkflowJob {
    pub id: u64,
    pub run_id: u64,
    pub run_attempt: Option<u32>,
    pub head_sha: String,
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub runner_name: Option<String>,
    #[serde(default)]
    pub runner_id: Option<u64>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct VerifiedJob {
    pub base_repository: RepositoryRef,
    pub config_repository: RepositoryRef,
    pub run: WorkflowRun,
    pub job: WorkflowJob,
    pub observation: JobObservation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobObservation {
    pub repository_id: u64,
    pub repository_owner: String,
    pub repository_name: String,
    pub run_id: u64,
    pub run_attempt: u32,
    pub job_id: u64,
    pub head_sha: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub run_status: String,
    pub run_conclusion: Option<String>,
    pub runner_name: Option<String>,
    pub runner_id: Option<u64>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

#[derive(Clone)]
pub struct GitHubClient {
    client: Client,
    origin: Url,
    token_file: String,
    timeout: Duration,
}

mod authenticated;
mod delivery;
mod intake;
mod pool_expiry;
mod reruns;

enum SnapshotFailure {
    Dependency,
    Rejected(&'static str),
}

fn snapshot_image_catalogs(config: &ConfigPatch, alias: &str) -> Vec<Value> {
    let mut catalogs = Vec::new();
    for catalog in config.aws_catalog_bindings.as_deref().unwrap_or_default() {
        if let Some(image) = catalog.images.get(alias) {
            catalogs.push(json!({"cloud":"aws","catalog_id":catalog.id,"provider_binding_id":catalog.provider_binding_id,"alias":alias,"image":image}));
        }
    }
    for catalog in config.azure_catalog_bindings.as_deref().unwrap_or_default() {
        if let Some(image) = catalog.images.get(alias) {
            catalogs.push(json!({"cloud":"azure","catalog_id":catalog.id,"provider_binding_id":catalog.provider_binding_id,"alias":alias,"image":image}));
        }
    }
    for catalog in config.gcp_catalog_bindings.as_deref().unwrap_or_default() {
        if let Some(image) = catalog.images.get(alias) {
            catalogs.push(json!({"cloud":"gcp","catalog_id":catalog.id,"provider_binding_id":catalog.provider_binding_id,"alias":alias,"image":image}));
        }
    }
    catalogs
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn attempt_or_zero(job: &WorkflowJobRef) -> u32 {
    job.run_attempt.unwrap_or_default()
}

#[derive(Deserialize)]
struct CommitResponse {
    sha: String,
}

#[derive(Deserialize)]
struct ContentsResponse {
    encoding: String,
    size: usize,
    content: String,
}

#[derive(Deserialize)]
struct CheckRunRef {
    external_id: Option<String>,
}

fn same_job_identity(
    job: &WorkflowJob,
    event: &WorkflowJobEvent,
    attempt: u32,
    run: &WorkflowRun,
) -> bool {
    job.id == event.workflow_job.id
        && job.run_id == run.id
        && job.run_attempt == Some(attempt)
        && job.head_sha == run.head_sha
        && job.head_sha == event.workflow_job.head_sha
}

fn valid_job_state(status: &str, conclusion: Option<&str>) -> bool {
    match status {
        "queued" | "waiting" | "in_progress" => conclusion.is_none(),
        "completed" => matches!(
            conclusion,
            Some(
                "success"
                    | "failure"
                    | "cancelled"
                    | "skipped"
                    | "timed_out"
                    | "action_required"
                    | "neutral"
                    | "stale"
            )
        ),
        _ => false,
    }
}

fn valid_run_state(run: &WorkflowRun) -> bool {
    match run.status.as_str() {
        "queued" | "in_progress" | "waiting" | "requested" | "pending" => run.conclusion.is_none(),
        "completed" => matches!(
            run.conclusion.as_deref(),
            Some(
                "success"
                    | "failure"
                    | "cancelled"
                    | "skipped"
                    | "timed_out"
                    | "action_required"
                    | "neutral"
                    | "stale"
            )
        ),
        _ => false,
    }
}

fn valid_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn path_component(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn repository_path(repository: &RepositoryRef) -> String {
    format!(
        "{}/{}",
        path_component(&repository.owner.login),
        path_component(&repository.name)
    )
}

fn next_link(headers: &reqwest::header::HeaderMap) -> Result<Option<Url>, GitHubError> {
    let Some(link) = headers.get(reqwest::header::LINK) else {
        return Ok(None);
    };
    let link = link.to_str().map_err(|_| GitHubError::Request)?;
    let mut next_link = None;
    for entry in link.split(',') {
        let mut fields = entry.trim().split(';');
        let url = fields
            .next()
            .ok_or(GitHubError::Request)?
            .trim()
            .strip_prefix('<')
            .and_then(|value| value.strip_suffix('>'))
            .ok_or(GitHubError::Request)?;
        let is_next =
            fields.any(|field| field.trim().trim_start_matches("rel=").trim_matches('"') == "next");
        if is_next {
            next_link = Some(Url::parse(url).map_err(|_| GitHubError::Request)?);
        }
    }
    Ok(next_link)
}
