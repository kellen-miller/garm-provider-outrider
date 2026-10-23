use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::ConfigError;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunnerProfilePatch {
    pub os: Option<String>,
    pub architecture: Option<String>,
    pub image: Option<String>,
    pub vcpu_min: Option<u32>,
    pub vcpu_max: Option<u32>,
    pub memory_gib_min: Option<u32>,
    pub memory_gib_max: Option<u32>,
    pub machine_types: Option<BTreeMap<String, Vec<String>>>,
    pub machine_families: Option<BTreeMap<String, Vec<String>>>,
    pub accounts: Option<Vec<String>>,
    pub locations: Option<Vec<String>>,
    pub gpu: Option<GpuRequirementPatch>,
    pub boot_disk: Option<DiskRequirementPatch>,
    pub local_scratch_gib: Option<u32>,
    pub capabilities: Option<BTreeSet<String>>,
    pub purchasing: Option<String>,
    pub purchasing_models: Option<BTreeSet<String>>,
    pub purchasing_tiers: Option<Vec<Vec<String>>>,
    pub maximum_total_cost: Option<rust_decimal::Decimal>,
    pub policy: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GpuRequirement {
    pub vendor: String,
    pub model: Option<String>,
    pub count: u32,
    pub memory_gib: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GpuRequirementPatch {
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub count: Option<u32>,
    pub memory_gib: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiskRequirement {
    pub class: String,
    pub size_gib: u32,
    pub iops: Option<u32>,
    pub throughput_mib_s: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DiskRequirementPatch {
    pub class: Option<String>,
    pub size_gib: Option<u32>,
    pub iops: Option<u32>,
    pub throughput_mib_s: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RunnerProfile {
    pub os: String,
    pub architecture: String,
    pub image: String,
    pub vcpu_min: u32,
    pub vcpu_max: u32,
    pub memory_gib_min: u32,
    pub memory_gib_max: u32,
    pub machine_types: BTreeMap<String, Vec<String>>,
    pub machine_families: BTreeMap<String, Vec<String>>,
    pub accounts: Vec<String>,
    pub locations: Vec<String>,
    pub gpu: Option<GpuRequirement>,
    pub boot_disk: DiskRequirement,
    pub local_scratch_gib: u32,
    pub capabilities: BTreeSet<String>,
    pub purchasing: String,
    pub purchasing_models: BTreeSet<String>,
    pub purchasing_tiers: Vec<Vec<String>>,
    pub maximum_total_cost: Option<rust_decimal::Decimal>,
    pub policy: String,
    pub cost_horizon_seconds: u64,
    pub max_price_age_seconds: u64,
    pub administrator_limits: AdministratorLimits,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdministratorLimits {
    pub max_vcpu: u32,
    pub max_memory_gib: u32,
    pub max_boot_disk_gib: u32,
    pub max_boot_disk_iops: u32,
    pub max_boot_disk_throughput_mib_s: u32,
    pub max_local_scratch_gib: u32,
    pub max_gpu_count: u32,
    pub max_gpu_memory_gib: u32,
    pub max_allocations_controller: u32,
    pub max_allocations_account: u32,
    pub max_allocations_pool: u32,
    pub max_total_cost: Option<rust_decimal::Decimal>,
    pub allowed_clouds: BTreeSet<String>,
    pub allowed_accounts: BTreeSet<String>,
    pub allowed_locations: BTreeSet<String>,
    pub allowed_machine_types: BTreeMap<String, BTreeSet<String>>,
    pub allowed_machine_families: BTreeMap<String, BTreeSet<String>>,
    pub allowed_gpu_vendors: BTreeSet<String>,
    pub allowed_gpu_models: BTreeSet<String>,
    pub allowed_os: BTreeSet<String>,
    pub allowed_architectures: BTreeSet<String>,
    pub allowed_images: BTreeSet<String>,
    pub allowed_capabilities: BTreeSet<String>,
    pub allowed_purchasing: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRequest {
    pub profile_name: String,
    pub profile: RunnerProfile,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JobOverrides {
    pub profile: RunnerProfilePatch,
    pub network_id: Option<String>,
    pub credentials: Option<String>,
    pub bootstrap_script: Option<String>,
}

impl RunnerProfilePatch {
    pub(super) fn apply(&mut self, patch: &Self) {
        macro_rules! replace {
            ($field:ident) => {
                if patch.$field.is_some() {
                    self.$field = patch.$field.clone();
                }
            };
        }

        replace!(os);
        replace!(architecture);
        replace!(image);
        replace!(vcpu_min);
        replace!(vcpu_max);
        replace!(memory_gib_min);
        replace!(memory_gib_max);
        replace!(accounts);
        replace!(locations);
        self.apply_gpu(&patch.gpu);
        self.apply_boot_disk(&patch.boot_disk);
        replace!(local_scratch_gib);
        replace!(capabilities);
        replace!(purchasing);
        replace!(purchasing_models);
        replace!(purchasing_tiers);
        replace!(maximum_total_cost);
        replace!(policy);
        merge_cloud_values(&mut self.machine_types, &patch.machine_types);
        merge_cloud_values(&mut self.machine_families, &patch.machine_families);
    }

    fn apply_gpu(&mut self, patch: &Option<GpuRequirementPatch>) {
        if let Some(patch) = patch {
            let current = self.gpu.get_or_insert_with(GpuRequirementPatch::default);
            if patch.vendor.is_some() {
                current.vendor = patch.vendor.clone();
            }

            if patch.model.is_some() {
                current.model = patch.model.clone();
            }

            if patch.count.is_some() {
                current.count = patch.count;
            }

            if patch.memory_gib.is_some() {
                current.memory_gib = patch.memory_gib;
            }
        }
    }

    fn apply_boot_disk(&mut self, patch: &Option<DiskRequirementPatch>) {
        if let Some(patch) = patch {
            let current = self
                .boot_disk
                .get_or_insert_with(DiskRequirementPatch::default);
            if patch.class.is_some() {
                current.class = patch.class.clone();
            }

            if patch.size_gib.is_some() {
                current.size_gib = patch.size_gib;
            }

            if patch.iops.is_some() {
                current.iops = patch.iops;
            }

            if patch.throughput_mib_s.is_some() {
                current.throughput_mib_s = patch.throughput_mib_s;
            }
        }
    }

    pub(super) fn finish(
        &self,
        cost_horizon_seconds: u64,
        max_price_age_seconds: u64,
        administrator_limits: &AdministratorLimits,
    ) -> Result<RunnerProfile, ConfigError> {
        let need = |name: &str| ConfigError::InvalidProfile(format!("missing {name}"));
        Ok(RunnerProfile {
            os: self.os.clone().ok_or_else(|| need("os"))?,
            architecture: self
                .architecture
                .clone()
                .ok_or_else(|| need("architecture"))?,
            image: self.image.clone().ok_or_else(|| need("image"))?,
            vcpu_min: self.vcpu_min.ok_or_else(|| need("vcpu_min"))?,
            vcpu_max: self.vcpu_max.unwrap_or(self.vcpu_min.unwrap_or_default()),
            memory_gib_min: self.memory_gib_min.ok_or_else(|| need("memory_gib_min"))?,
            memory_gib_max: self
                .memory_gib_max
                .unwrap_or(self.memory_gib_min.unwrap_or_default()),
            machine_types: self.machine_types.clone().unwrap_or_default(),
            machine_families: self.machine_families.clone().unwrap_or_default(),
            accounts: self.accounts.clone().unwrap_or_default(),
            locations: self.locations.clone().unwrap_or_default(),
            gpu: match &self.gpu {
                Some(gpu) => Some(GpuRequirement {
                    vendor: gpu.vendor.clone().ok_or_else(|| need("gpu.vendor"))?,
                    model: gpu.model.clone(),
                    count: gpu.count.ok_or_else(|| need("gpu.count"))?,
                    memory_gib: gpu.memory_gib,
                }),
                None => None,
            },
            boot_disk: match &self.boot_disk {
                Some(disk) => DiskRequirement {
                    class: disk.class.clone().ok_or_else(|| need("boot_disk.class"))?,
                    size_gib: disk.size_gib.ok_or_else(|| need("boot_disk.size_gib"))?,
                    iops: disk.iops,
                    throughput_mib_s: disk.throughput_mib_s,
                },
                None => return Err(need("boot_disk")),
            },
            local_scratch_gib: self.local_scratch_gib.unwrap_or(0),
            capabilities: self.capabilities.clone().unwrap_or_default(),
            purchasing: self.purchasing.clone().ok_or_else(|| need("purchasing"))?,
            purchasing_models: self
                .purchasing_models
                .clone()
                .ok_or_else(|| need("purchasing_models"))?,
            purchasing_tiers: self
                .purchasing_tiers
                .clone()
                .ok_or_else(|| need("purchasing_tiers"))?,
            maximum_total_cost: self
                .maximum_total_cost
                .or(administrator_limits.max_total_cost),
            policy: self.policy.clone().ok_or_else(|| need("policy"))?,
            cost_horizon_seconds,
            max_price_age_seconds,
            administrator_limits: administrator_limits.clone(),
        })
    }
}

fn merge_cloud_values(
    target: &mut Option<BTreeMap<String, Vec<String>>>,
    patch: &Option<BTreeMap<String, Vec<String>>>,
) {
    if let Some(fields) = patch {
        let current = target.get_or_insert_with(BTreeMap::new);
        for (cloud, values) in fields {
            current.insert(cloud.clone(), values.clone());
        }
    }
}
