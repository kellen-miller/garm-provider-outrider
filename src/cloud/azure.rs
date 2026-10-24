pub use billing::disk_meter_tier;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use azure_core::credentials::{Secret, TokenCredential};
use azure_identity::ClientSecretCredential;
use rust_decimal::Decimal;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    sync::Mutex,
    time::{Instant, timeout_at},
};

use super::{
    CatalogQuery, CloudCatalog, CloudError, CloudFailure, InterruptionEvidence, OwnershipScope,
    PlacementIdentity, ProviderInstance,
};
use crate::{
    config::{AzureAcceleratorFacts, AzureCatalogBinding, AzureImageBinding, AzureImageLicensing},
    policy::{
        AzureLaunchFacts, Candidate, CatalogOffer, PriceComponent, PriceQuote, ProviderLaunchFacts,
        ProviderSelection,
    },
    providers::ProviderBinding,
};

const MANAGEMENT_ORIGIN: &str = "https://management.azure.com";
const RESOURCE_SKUS_API: &str = "2021-07-01";
const COMPUTE_API: &str = "2025-04-01";
const RETAIL_API: &str = "2023-01-01-preview";
const MAX_OS_DISK_GIB: u32 = 4095;
const MAX_API_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

mod billing;
mod catalog;
mod connection;
mod discovery;
mod identity;
#[cfg(test)]
#[path = "azure/tests.rs"]
mod tests;

#[derive(Clone, Debug)]
pub struct AzureLocation {
    pub location: String,
    pub vnet_subnet_id: Option<String>,
    pub public_ip_sku: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AzureImage {
    pub image_id: String,
    pub minimum_os_disk_gib: u32,
    pub minimum_os_disk_source: String,
    pub capabilities: BTreeSet<String>,
    pub licensing: AzureImageLicensing,
    pub licensing_source: String,
}

#[derive(Clone, Debug)]
pub struct AzureSettings {
    pub catalog_id: String,
    pub account_id: String,
    pub subscription_id: String,
    pub pricing_location: String,
    pub locations: BTreeMap<String, AzureLocation>,
    pub images: BTreeMap<String, AzureImage>,
    pub storage_classes: BTreeMap<String, String>,
    pub accelerator_facts: BTreeMap<String, AzureAcceleratorFacts>,
    pub binding_id: String,
    pub source_revision: String,
    pub binary_sha256: String,
    pub configuration_sha256: String,
    pub provider_configuration_path: PathBuf,
    pub management_endpoint: String,
    pub retail_endpoint: String,
}

#[derive(Deserialize)]
struct AzureChildConfiguration {
    credentials: AzureChildCredentials,
    location: String,
    #[serde(default)]
    disable_isolated_networks: bool,
    #[serde(default)]
    vnet_subnet_id: String,
    #[serde(default)]
    use_ephemeral_storage: bool,
}

#[derive(Deserialize)]
struct AzureChildCredentials {
    subscription_id: String,
    #[serde(default)]
    client_options: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    service_principal: Option<AzureServicePrincipal>,
    #[serde(default)]
    workload_identity: Option<AzureUnsupportedIdentity>,
    #[serde(default)]
    managed_identity: Option<AzureUnsupportedIdentity>,
}

#[derive(Deserialize)]
struct AzureServicePrincipal {
    tenant_id: String,
    client_id: String,
    client_secret: String,
}

#[derive(Deserialize)]
struct AzureUnsupportedIdentity {}

#[derive(Deserialize)]
struct AzureSkuPage {
    #[serde(default)]
    value: Vec<AzureSku>,
    #[serde(default, rename = "nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureVmPage {
    #[serde(default)]
    value: Vec<AzureVm>,
    #[serde(default, rename = "nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureVm {
    id: String,
    name: String,
    location: String,
    #[serde(default)]
    tags: BTreeMap<String, String>,
    properties: AzureVmProperties,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureVmProperties {
    #[serde(default)]
    vm_id: Option<String>,
    #[serde(default)]
    instance_view: Option<AzureVmInstanceView>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureVmInstanceView {
    #[serde(default)]
    statuses: Vec<AzureVmStatus>,
}

#[derive(Deserialize)]
struct AzureVmStatus {
    code: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureResourceGraphPage {
    #[serde(default)]
    data: Vec<AzureResourceAnnotation>,
    result_truncated: Option<serde_json::Value>,
    #[serde(rename = "$skipToken")]
    skip_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureResourceAnnotation {
    id: String,
    subscription_id: String,
    properties: AzureAnnotationProperties,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureAnnotationProperties {
    target_resource_id: String,
    occurred_time: String,
    annotation_name: String,
    reason: String,
    summary: String,
    context: String,
    category: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureSku {
    name: String,
    #[serde(default)]
    family: Option<String>,
    resource_type: String,
    #[serde(default)]
    locations: Vec<String>,
    #[serde(default)]
    capabilities: Vec<AzureCapability>,
    #[serde(default)]
    restrictions: Vec<serde_json::Value>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureCapability {
    name: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureImageVersion {
    properties: AzureImageProperties,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureImageProperties {
    os_disk_image: AzureImageOsDisk,
    #[serde(default)]
    architecture: Option<String>,
    #[serde(default)]
    plan: Option<AzurePurchasePlan>,
}

type AzurePurchasePlan = BTreeMap<String, serde_json::Value>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureImageOsDisk {
    operating_system: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetailPage {
    #[serde(default, rename = "Items")]
    items: Vec<RetailMeter>,
    #[serde(default, rename = "NextPageLink")]
    next_page_link: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetailMeter {
    service_name: String,
    product_name: String,
    sku_name: String,
    arm_sku_name: String,
    meter_name: String,
    arm_region_name: String,
    unit_of_measure: String,
    #[serde(rename = "type")]
    kind: String,
    is_primary_meter_region: bool,
    effective_start_date: String,
    currency_code: String,
    tier_minimum_units: Decimal,
    retail_price: Decimal,
    unit_price: Decimal,
    meter_id: String,
}

struct AzureShape {
    sku: AzureSku,
    vcpu: u32,
    cpu_architecture: Option<String>,
    memory_gib: Decimal,
    gpu_count: u32,
    reported_gpu_memory_gib: Option<Decimal>,
    local_scratch_gib: Decimal,
    max_disk_iops: Option<u32>,
    max_disk_throughput_mib_s: Option<Decimal>,
    ephemeral_disk: bool,
    premium_io_supported: Option<bool>,
}

struct AzurePriceSet {
    compute: RetailMeter,
    disk: RetailMeter,
    public_ip: Option<RetailMeter>,
    licensing_included: bool,
    licensing_source: String,
    licensing_description: String,
}

struct AzurePriceRequest<'a> {
    sku: &'a str,
    location: &'a str,
    os: &'a str,
    purchasing: &'a str,
    disk_sku: &'a str,
    disk_gib: u32,
    licensing: AzureImageLicensing,
    licensing_source: &'a str,
    public_requested: bool,
}

pub struct AzureBackend {
    settings: AzureSettings,
    client: reqwest::Client,
    credential: Arc<dyn TokenCredential>,
    child: AzureChildConfiguration,
    retail_snapshots: Mutex<BTreeMap<String, AzureRetailSnapshot>>,
}

#[derive(Clone)]
struct AzureRetailSnapshot {
    meters: Arc<Vec<RetailMeter>>,
    observed_at: Instant,
    observed_at_unix_seconds: u64,
    fetched_at: Instant,
    machine_types: BTreeSet<String>,
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn failure(kind: &str, message: &str) -> CloudError {
    CloudError::Failure(CloudFailure {
        kind: kind.to_owned(),
        message: message.to_owned(),
        partial_resources: vec![],
        provider_request_id: None,
    })
}
