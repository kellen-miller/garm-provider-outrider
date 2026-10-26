use super::{
    AwsImageBinding, AzureAcceleratorFacts, AzureImageBinding, GcpAcceleratorFacts,
    GcpDiskPerformance, GcpImageBinding, GcpMachineFacts,
};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwsCatalogSource {
    pub id: String,
    pub provider_binding_id: String,
    pub account_id: String,
    pub pricing_location: String,
    pub locations: BTreeMap<String, AwsNetworkSource>,
    pub images: BTreeMap<String, AwsImageBinding>,
    pub storage_classes: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwsNetworkSource {
    pub availability_zone: String,
    pub security_group_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AzureCatalogSource {
    pub id: String,
    pub provider_binding_id: String,
    pub locations: BTreeMap<String, AzureNetworkSource>,
    pub images: BTreeMap<String, AzureImageBinding>,
    pub storage_classes: BTreeMap<String, String>,
    pub accelerator_facts: BTreeMap<String, AzureAcceleratorFacts>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AzureNetworkSource {
    pub public_ip_sku: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcpCatalogSource {
    pub id: String,
    pub provider_binding_id: String,
    pub images: BTreeMap<String, GcpImageBinding>,
    pub storage_classes: BTreeMap<String, String>,
    pub storage_billing_skus: BTreeMap<String, String>,
    pub external_ip_billing_sku: Option<String>,
    pub accelerator_facts: BTreeMap<String, GcpAcceleratorFacts>,
    pub machine_facts: BTreeMap<String, GcpMachineFacts>,
    pub disk_performance: BTreeMap<String, GcpDiskPerformance>,
    #[serde(default)]
    pub capacity_advice_enabled: bool,
}
