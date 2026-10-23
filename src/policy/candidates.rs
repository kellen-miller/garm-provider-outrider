use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::config::{AzureImageLicensing, RunnerProfile};

use super::{ObservationAges, ObservationWindow, PriceQuote};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub identity: String,
    pub os: String,
    pub architecture: String,
    pub image: String,
    pub vcpu: u32,
    pub memory_gib: Decimal,
    pub machine_type: String,
    pub machine_family: Option<String>,
    pub cloud: String,
    pub purchasing: String,
    pub account: String,
    pub location: String,
    pub gpu_vendor: Option<String>,
    pub gpu_model: Option<String>,
    pub gpu_count: u32,
    pub gpu_memory_gib: Option<Decimal>,
    pub boot_disk_class: String,
    pub boot_disk_gib: u32,
    pub boot_disk_iops: u32,
    pub boot_disk_throughput_mib_s: Decimal,
    pub local_scratch_gib: Decimal,
    pub capabilities: Vec<String>,
    pub price: Option<PriceQuote>,
    pub interruption_rate_per_hour: Option<Decimal>,
    pub allocation_failure_rate: Option<Decimal>,
    pub mean_create_seconds: Option<Decimal>,
    pub observation_age_seconds: Option<u64>,
    pub observation_ages: Option<ObservationAges>,
    pub observation_window: Option<ObservationWindow>,
    pub observation_samples: u64,
    pub definitive_create_outcomes: u64,
    #[serde(default)]
    pub measured_latency_samples: u64,
    pub interruption_samples: u64,
    pub native_capacity_hint: Option<NativeCapacityHint>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderSelection {
    pub binding_id: String,
    pub source_revision: String,
    pub binary_sha256: String,
    pub configuration_sha256: String,
    pub flavor: String,
    pub image: String,
    pub approved_extras: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "provider",
    content = "facts",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProviderLaunchFacts {
    Aws(AwsLaunchFacts),
    Azure(AzureLaunchFacts),
    Gcp(GcpLaunchFacts),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GcpLaunchFacts {
    pub project_id: String,
    pub zone: String,
    pub network_id: String,
    pub subnetwork_id: String,
    pub external_ip_access: bool,
    pub image_id: String,
    pub image_minimum_disk_gib: u32,
    pub machine_type_accelerators: Vec<(String, u32)>,
    pub bundled_local_ssd_partitions: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AwsLaunchFacts {
    pub availability_zone: String,
    pub subnet_id: String,
    pub map_public_ip_on_launch: bool,
    pub root_device_name: String,
    pub root_volume_minimum_gib: u32,
    pub maximum_ebs_iops: u32,
    pub maximum_ebs_throughput_mib_s: Decimal,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AzureLaunchFacts {
    pub subscription_id: String,
    pub location: String,
    pub image_version: String,
    pub os_disk_sku: String,
    pub public_ip_sku: Option<String>,
    pub minimum_os_disk_gib: u32,
    pub minimum_os_disk_source: String,
    pub image_licensing: AzureImageLicensing,
    pub image_licensing_source: String,
    pub maximum_data_disk_count: Option<u32>,
    pub maximum_os_disk_iops: Option<u32>,
    pub maximum_os_disk_throughput_mib_s: Option<Decimal>,
    pub vm_size_capabilities: BTreeMap<String, String>,
    pub ephemeral_os_disk_supported: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatalogOffer {
    pub candidate: Candidate,
    pub selection: ProviderSelection,
    pub launch_facts: ProviderLaunchFacts,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NativeCapacityHint {
    pub source: String,
    pub cloud: String,
    pub account: String,
    pub location: String,
    pub observed_at_unix_seconds: u64,
    pub score: Decimal,
}

impl Candidate {
    /// Check whether this candidate satisfies a runner profile and its limits.
    ///
    /// # Errors
    ///
    /// Returns the first profile or administrator constraint the candidate
    /// fails.
    pub fn meets(&self, profile: &RunnerProfile) -> Result<(), &'static str> {
        self.meets_identity(profile)?;
        self.meets_compute(profile)?;
        self.meets_placement(profile)?;
        self.meets_gpu(profile)?;
        self.meets_storage(profile)?;
        Ok(())
    }

    fn meets_identity(&self, profile: &RunnerProfile) -> Result<(), &'static str> {
        let limits = &profile.administrator_limits;
        if self.os != profile.os || !limits.allowed_os.contains(&self.os) {
            return Err("OS mismatch");
        }

        if self.architecture != profile.architecture
            || !limits.allowed_architectures.contains(&self.architecture)
        {
            return Err("architecture mismatch");
        }

        if self.image != profile.image || !limits.allowed_images.contains(&self.image) {
            return Err("image mismatch");
        }

        if !limits.allowed_clouds.contains(&self.cloud) {
            return Err("cloud not permitted");
        }

        if !limits.allowed_purchasing.contains(&self.purchasing)
            || !profile.purchasing_models.contains(&self.purchasing)
        {
            return Err("purchasing model not permitted");
        }

        Ok(())
    }

    fn meets_compute(&self, profile: &RunnerProfile) -> Result<(), &'static str> {
        let limits = &profile.administrator_limits;
        if self.vcpu < profile.vcpu_min
            || self.vcpu > profile.vcpu_max
            || self.vcpu > limits.max_vcpu
        {
            return Err("vCPU below requirement");
        }

        if self.memory_gib < Decimal::ZERO
            || self.memory_gib < Decimal::from(profile.memory_gib_min)
            || self.memory_gib > Decimal::from(profile.memory_gib_max)
            || self.memory_gib > Decimal::from(limits.max_memory_gib)
        {
            return Err("memory outside allowed range");
        }

        Ok(())
    }

    fn meets_placement(&self, profile: &RunnerProfile) -> Result<(), &'static str> {
        let limits = &profile.administrator_limits;
        if !limits.allowed_accounts.contains(&self.account)
            || (!profile.accounts.is_empty() && !profile.accounts.contains(&self.account))
        {
            return Err("account not permitted");
        }

        if !limits.allowed_locations.contains(&self.location)
            || (!profile.locations.is_empty() && !profile.locations.contains(&self.location))
        {
            return Err("location not permitted");
        }

        if limits
            .allowed_machine_types
            .get(&self.cloud)
            .is_none_or(|types| !types.contains(&self.machine_type))
        {
            return Err("administrator machine type restriction");
        }

        if let Some(types) = profile.machine_types.get(&self.cloud)
            && !types.contains(&self.machine_type)
        {
            return Err("machine type not permitted");
        }

        if let Some(families) = profile.machine_families.get(&self.cloud)
            && self
                .machine_family
                .as_ref()
                .is_none_or(|family| !families.contains(family))
        {
            return Err("machine family not permitted");
        }

        if limits
            .allowed_machine_families
            .get(&self.cloud)
            .is_some_and(|families| {
                self.machine_family
                    .as_ref()
                    .is_none_or(|family| !families.contains(family))
            })
        {
            return Err("administrator machine family restriction");
        }

        Ok(())
    }

    fn meets_gpu(&self, profile: &RunnerProfile) -> Result<(), &'static str> {
        let limits = &profile.administrator_limits;
        let requested_gpu_not_permitted = self.gpu_count > 0
            && (self
                .gpu_vendor
                .as_ref()
                .is_none_or(|vendor| !limits.allowed_gpu_vendors.contains(vendor))
                || (!limits.allowed_gpu_models.is_empty()
                    && self
                        .gpu_model
                        .as_ref()
                        .is_none_or(|model| !limits.allowed_gpu_models.contains(model))));
        if self.gpu_count > limits.max_gpu_count
            || (self.gpu_count > 0 && self.gpu_memory_gib.is_none())
            || (self.gpu_count > 0
                && self
                    .gpu_memory_gib
                    .is_some_and(|memory| memory <= Decimal::ZERO))
            || self
                .gpu_memory_gib
                .is_some_and(|memory| memory < Decimal::ZERO)
            || self
                .gpu_memory_gib
                .is_some_and(|memory| memory > Decimal::from(limits.max_gpu_memory_gib))
            || requested_gpu_not_permitted
        {
            return Err("GPU not permitted by administrator");
        }

        if let Some(gpu) = &profile.gpu {
            if self.gpu_vendor.as_deref() != Some(gpu.vendor.as_str()) {
                return Err("GPU vendor mismatch");
            }

            if gpu
                .model
                .as_deref()
                .is_some_and(|model| self.gpu_model.as_deref() != Some(model))
            {
                return Err("GPU model mismatch");
            }

            if self.gpu_count < gpu.count
                || self.gpu_memory_gib.unwrap_or_default()
                    < Decimal::from(gpu.memory_gib.unwrap_or_default())
                || self.gpu_count > limits.max_gpu_count
                || self
                    .gpu_memory_gib
                    .is_some_and(|memory| memory > Decimal::from(limits.max_gpu_memory_gib))
            {
                return Err("GPU capacity below requirement");
            }

            if gpu.count > limits.max_gpu_count
                || gpu.memory_gib.unwrap_or_default() > limits.max_gpu_memory_gib
                || !limits.allowed_gpu_vendors.contains(&gpu.vendor)
                || gpu
                    .model
                    .as_ref()
                    .is_some_and(|model| !limits.allowed_gpu_models.contains(model))
            {
                return Err("GPU not permitted by administrator");
            }
        }

        Ok(())
    }

    fn meets_storage(&self, profile: &RunnerProfile) -> Result<(), &'static str> {
        let limits = &profile.administrator_limits;
        if self.boot_disk_class != profile.boot_disk.class
            || self.boot_disk_gib < profile.boot_disk.size_gib
            || self.boot_disk_gib > limits.max_boot_disk_gib
            || self.boot_disk_iops < profile.boot_disk.iops.unwrap_or_default()
            || self.boot_disk_iops > limits.max_boot_disk_iops
            || self.boot_disk_throughput_mib_s
                < Decimal::from(profile.boot_disk.throughput_mib_s.unwrap_or_default())
            || self.boot_disk_throughput_mib_s
                > Decimal::from(limits.max_boot_disk_throughput_mib_s)
        {
            return Err("boot disk mismatch");
        }

        if self.local_scratch_gib < Decimal::ZERO
            || self.local_scratch_gib < Decimal::from(profile.local_scratch_gib)
            || self.local_scratch_gib > Decimal::from(limits.max_local_scratch_gib)
        {
            return Err("local scratch outside allowed range");
        }

        if !profile
            .capabilities
            .iter()
            .all(|capability| self.capabilities.contains(capability))
        {
            return Err("capability unavailable");
        }

        Ok(())
    }
}
