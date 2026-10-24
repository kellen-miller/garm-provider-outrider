use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

use google_cloud_auth::credentials::AccessTokenCredentials;
use reqwest::Url;
use rust_decimal::Decimal;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::time::{Instant, timeout_at};

use super::{
    CatalogQuery, CloudCatalog, CloudError, CloudFailure, InterruptionEvidence, OwnershipScope,
    PlacementIdentity, ProviderInstance,
};
use crate::{
    config::{
        GcpAcceleratorFacts, GcpCatalogBinding, GcpDiskPerformance, GcpImageBinding,
        GcpLicenseBasis, GcpLicenseFacts, GcpMachineFacts,
    },
    policy::{
        Candidate, CatalogOffer, GcpLaunchFacts, PriceComponent, PriceQuote, ProviderLaunchFacts,
        ProviderSelection,
    },
    providers::ProviderBinding,
};

const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const COMPUTE_ORIGIN: &str = "https://compute.googleapis.com";
const BILLING_ORIGIN: &str = "https://cloudbilling.googleapis.com";
const MAX_API_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

mod billing;
mod catalog;
mod connection;
mod discovery;
mod identity;
mod inventory;
#[cfg(test)]
#[path = "gcp/tests.rs"]
mod tests;

#[derive(Clone, Debug)]
pub struct GcpImage {
    pub image_id: String,
    pub os: String,
    pub os_source: String,
    pub architecture: String,
    pub capabilities: BTreeSet<String>,
    pub capabilities_source: String,
    pub license_free_source: Option<String>,
    pub license_facts: BTreeMap<String, GcpLicenseFacts>,
}

#[derive(Clone, Debug)]
pub struct GcpSettings {
    pub catalog_id: String,
    pub project_id: String,
    pub zone: String,
    pub pricing_location: String,
    pub images: BTreeMap<String, GcpImage>,
    pub storage_classes: BTreeMap<String, String>,
    pub storage_billing_skus: BTreeMap<String, String>,
    pub external_ip_billing_sku: Option<String>,
    pub accelerator_facts: BTreeMap<String, GcpAcceleratorFacts>,
    pub machine_facts: BTreeMap<String, GcpMachineFacts>,
    pub disk_performance: BTreeMap<String, GcpDiskPerformance>,
    pub capacity_advice_enabled: bool,
    pub binding_id: String,
    pub source_revision: String,
    pub binary_sha256: String,
    pub configuration_sha256: String,
    pub purchasing_capabilities: BTreeSet<String>,
    pub provider_configuration_path: PathBuf,
    pub compute_endpoint: String,
    pub billing_endpoint: String,
}

#[derive(Deserialize)]
struct GcpChildConfiguration {
    project_id: String,
    zone: String,
    credentials_file: String,
    network_id: String,
    subnetwork_id: String,
    #[serde(default)]
    external_ip_access: bool,
    #[serde(default)]
    enable_regional_placement: bool,
}

#[derive(Deserialize)]
struct GcpCredentialsFile {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(bound(deserialize = "T: serde::Deserialize<'de>"))]
struct ComputeList<T> {
    #[serde(default)]
    items: Vec<T>,
    next_page_token: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpOperation {
    id: String,
    name: String,
    operation_type: String,
    target_link: String,
    target_id: String,
    status: String,
    start_time: Option<String>,
    end_time: Option<String>,
    error: Option<serde_json::Value>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpMachineType {
    name: String,
    guest_cpus: u32,
    memory_mb: u32,
    #[serde(default)]
    architecture: Option<String>,
    #[serde(default)]
    is_shared_cpu: bool,
    #[serde(default)]
    accelerators: Vec<GcpMachineAccelerator>,
    #[serde(default)]
    bundled_local_ssds: Option<GcpBundledLocalSsd>,
    #[serde(default)]
    deprecated: Option<GcpDeprecated>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpMachineAccelerator {
    guest_accelerator_type: String,
    guest_accelerator_count: u32,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpBundledLocalSsd {
    partition_count: u32,
}

#[derive(Clone, Deserialize)]
struct GcpDeprecated {
    state: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpImageResource {
    self_link: String,
    architecture: Option<String>,
    disk_size_gb: Option<String>,
    status: String,
    #[serde(default)]
    licenses: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpDiskType {
    name: String,
    self_link: String,
    valid_disk_size: Option<String>,
    deprecated: Option<GcpDeprecated>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcpInstance {
    id: String,
    name: String,
    status: String,
    zone: String,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    metadata: Option<GcpMetadata>,
}

#[derive(Clone, Deserialize)]
struct GcpMetadata {
    #[serde(default)]
    items: Vec<GcpMetadataItem>,
}

#[derive(Clone, Deserialize)]
struct GcpMetadataItem {
    key: String,
    value: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingSkuPage {
    #[serde(default)]
    skus: Vec<BillingSku>,
    next_page_token: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingSku {
    name: String,
    description: String,
    category: BillingCategory,
    service_regions: Vec<String>,
    #[serde(default)]
    geo_taxonomy: Option<BillingGeoTaxonomy>,
    pricing_info: Vec<BillingPricingInfo>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum BillingGeoType {
    Global,
    #[serde(other)]
    Other,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingGeoTaxonomy {
    r#type: BillingGeoType,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingCategory {
    resource_family: String,
    resource_group: String,
    usage_type: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct BillingPricingInfo {
    effective_time: String,
    pricing_expression: BillingExpression,
    #[serde(default)]
    aggregation_info: Option<BillingAggregationInfo>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct BillingExpression {
    usage_unit: String,
    #[serde(default)]
    tiered_rates: Vec<BillingTier>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct BillingAggregationInfo {
    #[serde(default)]
    aggregation_level: Option<String>,
    #[serde(default)]
    aggregation_interval: Option<String>,
    #[serde(default)]
    aggregation_count: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct BillingTier {
    start_usage_amount: Decimal,
    unit_price: BillingMoney,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct BillingMoney {
    currency_code: String,
    units: String,
    nanos: i32,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CapacityAdviceRequest {
    instance_properties: CapacityInstanceProperties,
    instance_flexibility_policy: CapacityFlexibilityPolicy,
    size: u32,
    distribution_policy: CapacityDistributionPolicy,
}

#[derive(serde::Serialize)]
struct CapacityInstanceProperties {
    scheduling: CapacityScheduling,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CapacityScheduling {
    provisioning_model: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CapacityFlexibilityPolicy {
    instance_selections: CapacityInstanceSelections,
}

#[derive(serde::Serialize)]
struct CapacityInstanceSelections {
    candidate: CapacityCandidate,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CapacityCandidate {
    machine_types: Vec<String>,
    guest_accelerators: Vec<CapacityAccelerator>,
    disks: Vec<CapacityDisk>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CapacityAccelerator {
    accelerator_type: String,
    accelerator_count: u32,
}

#[derive(serde::Serialize)]
struct CapacityDisk {
    r#type: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CapacityDistributionPolicy {
    zones: Vec<CapacityZone>,
    target_shape: String,
}

#[derive(serde::Serialize)]
struct CapacityZone {
    zone: String,
}

#[derive(Deserialize)]
struct CapacityAdviceResponse {
    #[serde(default)]
    recommendations: Vec<CapacityRecommendation>,
}

#[derive(Deserialize)]
struct CapacityRecommendation {
    scores: Option<CapacityScores>,
    #[serde(default)]
    shards: Vec<CapacityShard>,
}

#[derive(Deserialize)]
struct CapacityScores {
    obtainability: Option<Decimal>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CapacityShard {
    zone: String,
    machine_type: String,
    instance_count: u32,
    provisioning_model: String,
}

struct GcpPriceSet {
    compute: Decimal,
    compute_source: String,
    compute_effective_at: u64,
    disk: Decimal,
    local_ssd: Decimal,
    disk_source: String,
    disk_effective_at: u64,
    license: Decimal,
    license_source: String,
    license_effective_at: Option<u64>,
    public_ip: Decimal,
    public_ip_source: Option<String>,
    public_ip_effective_at: Option<u64>,
    request_ids: Vec<String>,
}

struct GcpLicensePrice {
    hourly_rate: Decimal,
    source: String,
    effective_at: Option<u64>,
}

struct GcpCatalogMachineFacts {
    image: GcpImage,
    image_resource: GcpImageResource,
    minimum_disk_gib: u32,
    boot_disk_gib: u32,
    memory_gib: Decimal,
}

pub struct GcpBackend {
    settings: GcpSettings,
    client: reqwest::Client,
    credentials: AccessTokenCredentials,
    child: GcpChildConfiguration,
}

fn unique_sku(
    skus: &[BillingSku],
    matches: impl Fn(&BillingSku) -> bool,
) -> Result<&BillingSku, CloudError> {
    let mut selected = skus.iter().filter(|sku| matches(sku));
    let sku = selected
        .next()
        .ok_or_else(|| failure("pricing", "GCP Cloud Billing has no exact matching SKU"))?;
    if selected.next().is_some() {
        return Err(failure(
            "pricing",
            "GCP Cloud Billing SKU selection is ambiguous",
        ));
    }
    Ok(sku)
}
/// Resolve a configured GCP image name or immutable resource path.
///
/// # Errors
///
/// Returns a cloud error when the image ID is not a valid immutable image.
pub(crate) fn image_path_parts<'a>(
    project_id: &'a str,
    image_id: &'a str,
) -> Result<(&'a str, &'a str), CloudError> {
    if let Some(rest) = image_id.strip_prefix("projects/") {
        let (project, suffix) = rest.split_once("/global/images/").ok_or_else(|| {
            failure(
                "configuration",
                "GCP image ID is not an immutable image resource",
            )
        })?;
        if project.is_empty() || suffix.is_empty() || suffix.contains('/') {
            return Err(failure("configuration", "GCP image ID is invalid"));
        }
        Ok((project, suffix))
    } else if !image_id.is_empty() && !image_id.contains('/') {
        Ok((project_id, image_id))
    } else {
        Err(failure("configuration", "GCP image ID is invalid"))
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn failure(kind: &str, message: &str) -> CloudError {
    CloudError::Failure(CloudFailure {
        kind: kind.to_owned(),
        message: message.to_owned(),
        partial_resources: Vec::new(),
        provider_request_id: None,
    })
}
