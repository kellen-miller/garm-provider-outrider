use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use rusqlite::Connection;
use serde::Serialize;

const RETAINED_CONFIG_ALLOCATION_PREDICATE: &str =
    "dispatched_at IS NOT NULL AND state IN ('dispatched','ambiguous','created','deleting')";

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("daemon for this database is already running")]
    AlreadyRunning,
    #[error("journal operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("journal lock failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal mutex poisoned")]
    Poisoned,
    #[error("immutable ingress snapshot conflicts with its existing record")]
    SnapshotConflict,
    #[error("allocation identity was already deleted before reservation")]
    AllocationIdentityDeleted,
    #[error("allocation pool does not match the requested pool")]
    AllocationPoolMismatch,
    #[error("rerun intent state is invalid")]
    InvalidRerunState,
}

#[derive(Debug)]
pub enum AllocationDeleteTarget {
    Owned(Box<AllocationRecord>),
    Absent,
}

fn cleanup_backoff_seconds(attempt_count: i64) -> i64 {
    let shift = u32::try_from(attempt_count.saturating_sub(1).min(9)).unwrap_or(9);
    1_i64.checked_shl(shift).unwrap_or(300).min(300)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationRecord {
    pub controller_id: String,
    pub instance_id: String,
    pub command: String,
    pub state: String,
    pub tombstone: bool,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookDelivery {
    pub delivery_id: String,
    pub repository_id: u64,
    pub event: String,
    pub headers_json: String,
    pub body: Vec<u8>,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxMessage {
    pub id: String,
    pub kind: String,
    pub destination: String,
    pub headers_json: String,
    pub body: Vec<u8>,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolSnapshot {
    pub id: String,
    pub repository_id: u64,
    pub request_token: String,
    pub config_json: String,
    pub state: String,
    pub garm_pool_id: Option<String>,
    pub expires_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobObservationRecord {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RerunIntent {
    pub repository_id: u64,
    pub owner: String,
    pub repository: String,
    pub github_api_origin: String,
    pub run_id: u64,
    pub run_attempt: u32,
    pub job_id: u64,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRerunReservation {
    pub repository_id: u64,
    pub owner: String,
    pub repository: String,
    pub github_api_origin: String,
    pub run_id: u64,
    pub run_attempt: u32,
    pub job_id: u64,
    pub max_per_run: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedFailedJob {
    pub repository_id: u64,
    pub repository_owner: String,
    pub repository_name: String,
    pub run_id: u64,
    pub run_attempt: u32,
    pub job_id: u64,
    pub head_sha: String,
    pub runner_name: Option<String>,
    pub runner_id: Option<u64>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AllocationRecord {
    pub allocation_id: String,
    pub controller_id: String,
    pub runner_name: String,
    pub snapshot_id: String,
    pub catalog_fingerprint: Option<String>,
    pub pool_id: String,
    pub account_id: Option<String>,
    pub state: String,
    pub selected_offer_json: Option<String>,
    pub binding_json: Option<String>,
    pub provider_instance_json: Option<String>,
    pub native_identity_json: Option<String>,
    pub detail: Option<String>,
    pub created_at: u64,
    pub dispatched_at: Option<u64>,
    pub returned_at: Option<u64>,
    pub tombstone: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterruptionRecord {
    pub allocation_id: String,
    pub controller_id: String,
    pub pool_id: String,
    pub catalog_fingerprint: String,
    pub configuration_fingerprint: String,
    pub provider_pin_json: String,
    pub identity_json: String,
    pub source_api: String,
    pub api_version: String,
    /// Unmodified identifier returned by the provider.
    pub source_id: String,
    /// Stable provider revision key used for deduplication.
    pub event_id: String,
    pub occurred_at: String,
    pub observed_at: String,
    pub reason_fields_json: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateHistoryRecord {
    pub allocation_id: String,
    pub candidate_key: String,
    pub catalog_fingerprint: String,
    pub provider_pin_json: String,
    pub observed_at_millis: u128,
    pub latency_nanos: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningHistoryRecord {
    pub allocation_id: String,
    pub candidate_key: String,
    pub catalog_fingerprint: String,
    pub provider_pin_json: String,
    pub identity_json: String,
    pub session_id: String,
    pub observed_at_millis: u128,
    pub running: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateHistory {
    pub create_latencies_nanos: Vec<u64>,
    pub definitive_create_outcomes: u64,
    pub measured_latency_samples: u64,
    pub interruptions: u64,
    pub running_exposure_millis: u128,
    pub latest_observation_millis: Option<u128>,
    pub latest_create_millis: Option<u128>,
    pub latest_latency_millis: Option<u128>,
    pub latest_interruption_millis: Option<u128>,
    pub observation_samples: u64,
    pub latency_coverage_complete: bool,
    pub running_coverage_complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownCreateAttempt {
    pub offer_json: String,
    pub binding_json: String,
    pub catalog_fingerprint: Option<String>,
    pub observed_at_millis: Option<u64>,
}

fn same_interruption_source_record(left: &InterruptionRecord, right: &InterruptionRecord) -> bool {
    left.allocation_id == right.allocation_id
        && left.controller_id == right.controller_id
        && left.pool_id == right.pool_id
        && left.catalog_fingerprint == right.catalog_fingerprint
        && left.configuration_fingerprint == right.configuration_fingerprint
        && left.provider_pin_json == right.provider_pin_json
        && left.identity_json == right.identity_json
        && left.source_api == right.source_api
        && left.api_version == right.api_version
        && left.source_id == right.source_id
        && left.event_id == right.event_id
        && left.occurred_at == right.occurred_at
        && left.reason_fields_json == right.reason_fields_json
}

fn same_interruption_provenance(
    existing: &InterruptionRecord,
    incoming: &InterruptionRecord,
    identity_json: &str,
) -> bool {
    same_interruption_source_record(existing, incoming) && existing.identity_json == identity_json
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationActionRecord {
    pub action_id: i64,
    pub allocation_id: String,
    pub command: String,
    pub state: String,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub detail: Option<String>,
    pub unresolved_create: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationActionAdmission {
    pub allocation: AllocationRecord,
    pub action_id: Option<i64>,
    pub acquired: bool,
    pub unresolved_create: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChildAttemptRecord {
    pub allocation_id: String,
    pub attempt_number: u32,
    pub state: String,
    pub offer_json: String,
    pub binding_json: String,
    pub detail: Option<String>,
    pub created_at: u64,
    pub dispatched_at: Option<u64>,
    pub completed_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CleanupGeneration {
    pub action_id: i64,
    pub command: String,
    pub state: String,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub attempt_count: u32,
    pub last_attempt_at: Option<u64>,
    pub next_attempt_at: Option<u64>,
    pub unresolved_create: bool,
    pub reusable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CleanupResolutionScope {
    pub allocation: AllocationRecord,
    pub snapshot_json: String,
    pub actions: Vec<CleanupGeneration>,
    pub child_attempts: Vec<ChildAttemptRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CleanupResolutionAudit {
    pub attestation_digest: String,
    pub evidence_reference: String,
    pub verified_at: u64,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllocationAdmission {
    pub snapshot_id: String,
    pub repository_id: u64,
    pub run_id: u64,
    pub run_attempt: u32,
    pub now: u64,
}

pub(crate) fn native_identity_key(identity: &crate::cloud::PlacementIdentity) -> String {
    let (account, location, resource) = if identity.cloud.eq_ignore_ascii_case("azure") {
        (
            identity.account_id.to_lowercase(),
            identity.location.to_lowercase(),
            identity.resource_id.to_lowercase(),
        )
    } else {
        (
            identity.account_id.clone(),
            identity.location.clone(),
            identity.resource_id.clone(),
        )
    };
    format!(
        "{}:{}{}:{}{}:{}{}",
        identity.cloud.len(),
        identity.cloud,
        account.len(),
        account,
        location.len(),
        location,
        resource
    )
}

pub struct Journal {
    connection: Mutex<Connection>,
    _lock: JournalLock,
}

struct JournalLock {
    file: File,
}

impl Drop for JournalLock {
    fn drop(&mut self) {
        if let Err(error) = FileExt::unlock(&self.file) {
            tracing::warn!(%error, "failed to explicitly release journal lock");
        }
    }
}

mod actions;
mod allocations;
mod cleanup;
mod history;
mod operations;
mod runs;
mod schema;
mod signed_io;
