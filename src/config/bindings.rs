use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GcpCatalogBinding {
    pub id: String,
    pub provider_binding_id: String,
    pub project_id: String,
    pub zone: String,
    pub pricing_location: String,
    pub credential_reference: String,
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GcpImageBinding {
    pub image_id: String,
    pub os: String,
    pub os_source: String,
    pub architecture: String,
    pub capabilities: BTreeSet<String>,
    pub capabilities_source: String,
    pub license_free_source: Option<String>,
    pub license_facts: BTreeMap<String, GcpLicenseFacts>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GcpLicenseFacts {
    pub billing_sku: Option<String>,
    pub basis: GcpLicenseBasis,
    pub source: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GcpLicenseBasis {
    Included,
    PerVcpu,
    PerInstance,
    SqlVcpuFloor,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GcpAcceleratorFacts {
    pub vendor: String,
    pub model: String,
    pub memory_gib: rust_decimal::Decimal,
    pub spot_billing_sku: String,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GcpMachineFacts {
    pub shared_cpu_billing_sku: Option<String>,
    pub bundled_local_ssd_gib: Option<u32>,
    pub bundled_local_ssd_partitions: Option<u32>,
    pub local_ssd_billing_sku: Option<String>,
    pub local_ssd_billing_quantity: Option<rust_decimal::Decimal>,
    pub max_pd_iops: Option<u32>,
    pub max_pd_throughput_mib_s: Option<rust_decimal::Decimal>,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GcpDiskPerformance {
    pub base_iops: u32,
    pub iops_per_gib: u32,
    pub maximum_iops: u32,
    pub base_throughput_mib_s: rust_decimal::Decimal,
    pub throughput_per_gib_mib_s: rust_decimal::Decimal,
    pub maximum_throughput_mib_s: rust_decimal::Decimal,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AwsCatalogBinding {
    pub id: String,
    pub provider_binding_id: String,
    pub account_id: String,
    pub region: String,
    pub pricing_location: String,
    pub credential_reference: String,
    pub locations: BTreeMap<String, AwsNetworkBinding>,
    pub images: BTreeMap<String, AwsImageBinding>,
    pub storage_classes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AwsNetworkBinding {
    pub availability_zone: String,
    pub subnet_id: String,
    pub security_group_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AwsImageBinding {
    pub image_id: String,
    pub os: String,
    pub architecture: String,
    pub capabilities: BTreeSet<String>,
    pub windows_license_included: bool,
    pub cloud_init: bool,
    pub ec2_launch_agent: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AzureCatalogBinding {
    pub id: String,
    pub provider_binding_id: String,
    pub account_id: String,
    pub subscription_id: String,
    pub pricing_location: String,
    pub credential_reference: String,
    pub locations: BTreeMap<String, AzureNetworkBinding>,
    pub images: BTreeMap<String, AzureImageBinding>,
    pub storage_classes: BTreeMap<String, String>,
    pub accelerator_facts: BTreeMap<String, AzureAcceleratorFacts>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AzureAcceleratorFacts {
    pub vendor: String,
    pub model: String,
    pub total_memory_gib: rust_decimal::Decimal,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AzureNetworkBinding {
    pub location: String,
    pub vnet_subnet_id: Option<String>,
    pub public_ip_sku: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AzureImageBinding {
    pub image_id: String,
    pub minimum_os_disk_gib: u32,
    pub minimum_os_disk_source: String,
    pub capabilities: BTreeSet<String>,
    pub licensing: AzureImageLicensing,
    pub licensing_source: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AzureImageLicensing {
    NoAdditionalSoftwareCharge,
    WindowsLicenseInComputeMeter,
    AdditionalUnpricedSoftware,
}
