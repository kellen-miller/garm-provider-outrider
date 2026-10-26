mod catalog;
mod cleanup;
mod create;
mod explain;
mod history;
mod inventory;
mod lifecycle;
mod native;
mod observation;
mod recovery;
mod selection;

#[cfg(test)]
#[path = "allocation/explain_tests.rs"]
mod explanation_report_tests;
#[cfg(test)]
#[path = "allocation/native_tests.rs"]
mod interruption_identity_tests;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::OsString,
    fs::File,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub use self::catalog::catalog_configuration_fingerprint;
pub use self::inventory::{CatalogInventory, InventoryReport};
pub use self::native::canonical_native_account_scope;
pub use crate::policy::{ExplanationGcpLicense, ExplanationLaunchFacts, FinancialPriceScope};
use serde_json::Value;
use tokio::time::Instant;

use crate::{
    bootstrap::{BootstrapPlatform, BootstrapProvider, BootstrapTarget, generate_provider_hook},
    cloud::{
        CatalogQuery, InterruptionEvidence, OwnershipScope, PlacementIdentity, aws::AwsBackend,
        azure::AzureBackend, gcp::GcpBackend,
    },
    config::{
        AdministratorLimits, ConfigPatch, EffectivePrice, EffectivePriceComponent, RunnerProfile,
    },
    garm::{BootstrapInstance, PoolSnapshotId, ProviderRequest},
    github::SnapshotDocument,
    journal::{
        AllocationAdmission, AllocationRecord, ChildAttemptRecord, CreateHistoryRecord,
        InterruptionRecord, Journal, JournalError, PoolSnapshot, RunningHistoryRecord,
    },
    policy::{CatalogOffer, ProviderLaunchFacts, ProviderSelection, RankingResult},
    providers::{
        LocalFailure, ProcessLimits, ProviderBinding, ProviderError, ProviderRegistry,
        ProviderRequirements, ProviderResult, PurchasingCapability,
    },
};

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn unix_now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[derive(Debug, thiserror::Error)]
pub enum AllocationError {
    #[error("allocation configuration is invalid")]
    Configuration,
    #[error("allocation snapshot is missing or unavailable")]
    Snapshot,
    #[error("allocation request conflicts with its durable identity")]
    Conflict,
    #[error("runner name was deleted before allocation reservation")]
    DeletedName,
    #[error("allocation identity is definitely absent")]
    NotFound,
    #[error("administrator allocation admission is closed")]
    AdmissionClosed,
    #[error("allocation concurrency limit reached")]
    Limit,
    #[error("allocation catalog is unavailable")]
    Catalog,
    #[error("no eligible provider candidate")]
    NoCandidate,
    #[error("allocation remains unresolved after provider dispatch")]
    Ambiguous,
    #[error("provider lifecycle operation failed")]
    Provider,
    #[error("allocation journal failed")]
    Journal,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExplainRequest {
    pub profile: Option<String>,
    pub snapshot_id: Option<String>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationReport {
    pub input_source: String,
    pub snapshot: Option<ExplanationSnapshot>,
    pub analyzed_at_unix_seconds: u64,
    pub cost_horizon_seconds: u64,
    pub normalized_requirements: ExplanationRequirements,
    pub queried_catalogs: Vec<ExplanationCatalogScope>,
    pub queried_catalog_count: usize,
    pub queried_catalogs_truncated: bool,
    pub observed_exclusions: Vec<ExplanationCatalogExclusion>,
    pub observed_exclusion_count: usize,
    pub observed_exclusions_truncated: bool,
    pub observed_exclusions_exhaustive: bool,
    pub policy_attempts: Vec<ExplanationPolicyAttempt>,
    pub policy_attempt_count: usize,
    pub policy_attempts_truncated: bool,
    pub applied_policy: Option<String>,
    pub fallback_from: Option<String>,
    pub policy_error: Option<String>,
    pub candidates: Vec<ExplanationCandidate>,
    pub candidate_count: usize,
    pub candidates_truncated: bool,
    pub quote_missing: bool,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationSnapshot {
    pub id: String,
    pub repository_id: u64,
    pub run_id: u64,
    pub run_attempt: u32,
    pub job_id: u64,
    pub commit_sha: String,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationRequirements {
    pub os: String,
    pub architecture: String,
    pub vcpu_min: u32,
    pub vcpu_max: u32,
    pub memory_gib_min: rust_decimal::Decimal,
    pub memory_gib_max: rust_decimal::Decimal,
    pub purchasing_tiers: Vec<Vec<String>>,
    pub machine_types: BTreeMap<String, Vec<String>>,
    pub machine_families: BTreeMap<String, Vec<String>>,
    pub accounts: Vec<String>,
    pub locations: Vec<String>,
    pub gpu: Option<crate::config::GpuRequirement>,
    pub boot_disk: crate::config::DiskRequirement,
    pub local_scratch_gib: u32,
    pub capabilities: Vec<String>,
    pub purchasing_models: Vec<String>,
    pub maximum_total_cost: Option<rust_decimal::Decimal>,
    pub administrator_max_vcpu: u32,
    pub administrator_max_memory_gib: u32,
    pub administrator_max_boot_disk_gib: u32,
    pub administrator_max_boot_disk_iops: u32,
    pub administrator_max_boot_disk_throughput_mib_s: u32,
    pub administrator_max_local_scratch_gib: u32,
    pub administrator_max_gpu_count: u32,
    pub administrator_max_gpu_memory_gib: u32,
    pub max_price_age_seconds: u64,
    pub policy: String,
    pub image: String,
    pub cost_horizon_seconds: u64,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationCatalogScope {
    pub cloud: String,
    pub account: String,
    pub text_truncated: bool,
    pub locations: Vec<String>,
    pub location_count: usize,
    pub locations_truncated: bool,
    pub native_locations: Vec<String>,
    pub native_location_count: usize,
    pub native_locations_truncated: bool,
    pub all_configured_locations: bool,
    pub machine_types: Vec<String>,
    pub machine_type_count: usize,
    pub machine_types_truncated: bool,
    pub image_alias: String,
    pub purchasing: Vec<String>,
    pub purchasing_count: usize,
    pub purchasing_truncated: bool,
    pub queried: bool,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationCatalogExclusion {
    pub catalog: String,
    pub reason: String,
    pub text_truncated: bool,
    pub observed_count: usize,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationPolicyAttempt {
    pub name: String,
    pub text_truncated: bool,
    pub outcome: String,
    pub reason: Option<String>,
    pub candidates: Vec<ExplanationPolicyCandidate>,
    pub candidate_count: usize,
    pub candidates_truncated: bool,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationPolicyCandidate {
    pub identity: String,
    pub text_truncated: bool,
    pub eligible: bool,
    pub reason: String,
    pub score: Option<rust_decimal::Decimal>,
    pub secondary_score: Option<rust_decimal::Decimal>,
    pub purchasing_tier: Option<usize>,
    pub price_age_seconds: Option<u64>,
    pub score_terms: Vec<crate::policy::ScoreTerm>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationCandidate {
    pub identity: String,
    pub text_truncated: bool,
    pub eligible: bool,
    pub reason: String,
    pub score: Option<rust_decimal::Decimal>,
    pub secondary_score: Option<rust_decimal::Decimal>,
    pub purchasing_tier: Option<usize>,
    pub price_age_seconds: Option<u64>,
    pub score_terms: Vec<crate::policy::ScoreTerm>,
    pub capabilities: Vec<String>,
    pub capability_count: usize,
    pub capabilities_truncated: bool,
    pub financial_price_scope: FinancialPriceScope,
    pub technical_evidence: Vec<ExplanationTechnicalEvidence>,
    pub technical_evidence_count: usize,
    pub technical_evidence_truncated: bool,
    pub price: Option<ExplanationPrice>,
    pub history: ExplanationHistory,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationTechnicalEvidence {
    pub name: String,
    pub value: String,
    pub value_truncated: bool,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationPrice {
    pub currency: String,
    pub compute: ExplanationPriceComponent,
    pub disk: ExplanationPriceComponent,
    pub licensing: ExplanationPriceComponent,
    pub public_address: ExplanationPriceComponent,
    pub new_host_minimum_commitment_seconds: Option<u64>,
    pub existing_host_remaining_commitment_seconds: Option<u64>,
    pub included_charge_names: Vec<String>,
    pub included_charge_count: usize,
    pub included_charge_names_truncated: bool,
    pub exclusions: Vec<String>,
    pub exclusion_count: usize,
    pub exclusions_truncated: bool,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationPriceComponent {
    pub hourly_rate: Option<rust_decimal::Decimal>,
    pub minimum_commitment_seconds: u64,
    pub source: String,
    pub source_truncated: bool,
    pub observed_at_unix_seconds: Option<u64>,
    pub effective_at_unix_seconds: Option<u64>,
    pub age_seconds: Option<u64>,
    pub included_in_compute: bool,
    pub exclusions: Vec<String>,
    pub exclusion_count: usize,
    pub exclusions_truncated: bool,
    pub effective_price_override: Option<ExplanationEffectivePriceOverride>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationEffectivePriceOverride {
    pub id: String,
    pub reference: String,
    pub valid_from_unix_seconds: u64,
    pub valid_until_unix_seconds: u64,
    pub original_hourly_rate: Option<rust_decimal::Decimal>,
    pub original_source: String,
    pub original_source_truncated: bool,
    pub original_observed_at_unix_seconds: Option<u64>,
    pub original_effective_at_unix_seconds: Option<u64>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationHistory {
    pub sample_count: u64,
    pub create_outcomes: u64,
    pub latency_samples: u64,
    pub interruption_samples: u64,
    pub mean_create_seconds: Option<rust_decimal::Decimal>,
    pub failure_rate: Option<rust_decimal::Decimal>,
    pub interruptions_per_hour: Option<rust_decimal::Decimal>,
    pub observation_ages: Option<crate::policy::ObservationAges>,
    pub observation_window: Option<crate::policy::ObservationWindow>,
    pub queried_window: Option<ExplanationHistoryQueryWindow>,
    pub unknown_reasons: Vec<String>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ExplanationHistoryQueryWindow {
    pub start_unix_seconds: u64,
    pub end_unix_seconds: u64,
}

enum CatalogBackend {
    Aws(AwsBackend),
    Azure(AzureBackend),
    Gcp(GcpBackend),
}

pub struct ConnectedCatalog {
    backend: CatalogBackend,
    configuration_fingerprint: String,
    catalog_id: String,
    binding_id: String,
    cloud: String,
    account_id: String,
    locations: BTreeSet<String>,
}

struct CatalogLocationScope {
    configured: String,
    native: String,
}

struct NativeIdentityLookup<'a> {
    controller_id: &'a str,
    pool_id: &'a str,
    offer: &'a CatalogOffer,
    configuration_fingerprint: &'a str,
    runner_name: &'a str,
    child_instance_id: Option<&'a str>,
    deadline: Instant,
}

#[derive(Default)]
struct NativeIdentityResolution {
    identity: Option<PlacementIdentity>,
    listed: usize,
    same_name: usize,
    first_rejection: Option<&'static str>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ProviderPinKey {
    binding_id: String,
    source_revision: String,
    executable_sha256: String,
    configuration_sha256: String,
}

struct AllocationExecution<'a> {
    request: &'a ProviderRequest,
    snapshot: &'a PoolSnapshot,
    document: &'a SnapshotDocument,
    bootstrap: &'a BootstrapInstance,
    profile: &'a RunnerProfile,
    allocation: &'a AllocationRecord,
    deadline: Instant,
}

struct ChildCreateResult {
    result: Result<ProviderResult, ProviderError>,
    elapsed_nanos: Option<u64>,
}

enum CreateOutcome {
    Created(Value),
    Tombstoned {
        allocation: Box<AllocationRecord>,
        deadline: Instant,
    },
}

/// Production allocation executor. Its catalog adapters and child registry are
/// required dependencies constructed from the administrator's pinned bindings.
pub struct AllocationService {
    journal: Arc<Journal>,
    config: ConfigPatch,
    limits: AdministratorLimits,
    catalogs: Vec<ConnectedCatalog>,
    provider_registry: ProviderRegistry,
    provider_bindings: HashMap<String, ProviderBinding>,
    process_environment: BTreeMap<OsString, OsString>,
    history_session_id: String,
    recovery_offset: AtomicUsize,
    lifecycle_recovery_offset: AtomicUsize,
    interruption_recovery_offset: AtomicUsize,
}

impl AllocationService {
    pub fn history_window_seconds(&self) -> u64 {
        self.config
            .history
            .as_ref()
            .map_or(0, |settings| settings.window_seconds)
    }

    /// Execute one provider request through durable allocation and pinned child routing.
    ///
    /// # Errors
    /// Returns a sanitized error when allocation policy, the journal, or the pinned
    /// provider process cannot establish a safe result.
    pub async fn handle(&self, request: ProviderRequest) -> Result<Option<Value>, AllocationError> {
        match request.command.as_str() {
            "CreateInstance" => match Box::pin(self.create(request)).await? {
                CreateOutcome::Created(instance) => Ok(Some(instance)),
                CreateOutcome::Tombstoned {
                    allocation,
                    deadline,
                } => {
                    // Let Create unwind before polling guarded cleanup to bound stack depth.
                    let cleanup = Box::pin(self.delete_allocation(&allocation, deadline)).await;
                    if let Err(error) = cleanup {
                        self.journal
                            .schedule_allocation_cleanup(
                                &allocation.allocation_id,
                                "dispatched create did not produce a usable provider result",
                            )
                            .map_err(|_| AllocationError::Journal)?;
                        self.journal
                            .close_unspawned_allocation(&allocation.allocation_id)
                            .map_err(|_| AllocationError::Journal)?;
                        return Err(error);
                    }
                    Err(AllocationError::Conflict)
                }
            },
            "ValidatePoolInfo" => self.validate_pool(&request).map(|()| None),
            "GetInstance" => Box::pin(self.get(request)).await.map(Some),
            "ListInstances" => Box::pin(self.list(request)).await.map(Some),
            "DeleteInstance" => Box::pin(self.delete(request)).await.map(|()| None),
            "StartInstance" | "StopInstance" => {
                Box::pin(self.start_stop(request)).await.map(|()| None)
            }
            "RemoveAllInstances" => Box::pin(self.remove_all(request)).await.map(|()| None),
            _ => Err(AllocationError::Provider),
        }
    }
}
