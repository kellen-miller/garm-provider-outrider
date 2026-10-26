use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::policy::{PolicyDefinition, validate_fallback_graph};
use crate::providers::ProviderBinding;

pub use bindings::{
    AwsCatalogBinding, AwsImageBinding, AwsNetworkBinding, AzureAcceleratorFacts,
    AzureCatalogBinding, AzureImageBinding, AzureImageLicensing, AzureNetworkBinding,
    GcpAcceleratorFacts, GcpCatalogBinding, GcpDiskPerformance, GcpImageBinding, GcpLicenseBasis,
    GcpLicenseFacts, GcpMachineFacts,
};
pub use profiles::{
    AdministratorLimits, DiskRequirement, DiskRequirementPatch, GpuRequirement,
    GpuRequirementPatch, JobOverrides, ResolvedRequest, RunnerProfile, RunnerProfilePatch,
};
pub use schema::{extra_specs_json_schema, json_schema};

mod bindings;
mod document;
mod profiles;
mod schema;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("invalid TOML: {0}")]
    Toml(String),
    #[error("unknown field {0}")]
    UnknownField(String),
    #[error("profile {0} is missing")]
    MissingProfile(String),
    #[error("override {0} is forbidden in repository/job configuration")]
    ForbiddenOverride(String),
    #[error("{0} is outside administrator limits")]
    HardLimit(String),
    #[error("invalid profile: {0}")]
    InvalidProfile(String),
    #[error("configuration version must be 1")]
    UnsupportedVersion,
}

fn deserialize_quoted_decimal<'de, D>(deserializer: D) -> Result<rust_decimal::Decimal, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    rust_decimal::Decimal::from_str_exact(&value).map_err(serde::de::Error::custom)
}

fn serialize_quoted_decimal<S>(
    value: &rust_decimal::Decimal,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.normalize().to_string())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfigPatch {
    pub version: Option<u32>,
    pub profiles: Option<BTreeMap<String, RunnerProfilePatch>>,
    pub defaults: Option<RunnerProfilePatch>,
    pub limits: Option<AdministratorLimits>,
    pub default_purchasing: Option<String>,
    pub default_policy: Option<String>,
    pub policies: Option<BTreeMap<String, PolicyDefinition>>,
    pub cost_horizon_seconds: Option<u64>,
    pub max_price_age_seconds: Option<u64>,
    pub history: Option<HistorySettings>,
    pub control: Option<ControlSettings>,
    pub admission: Option<AdmissionSettings>,
    pub provider_bindings: Option<Vec<ProviderBinding>>,
    pub aws_catalog_bindings: Option<Vec<AwsCatalogBinding>>,
    pub azure_catalog_bindings: Option<Vec<AzureCatalogBinding>>,
    pub gcp_catalog_bindings: Option<Vec<GcpCatalogBinding>>,
    pub effective_prices: Option<Vec<EffectivePrice>>,
    pub ingress: Option<IngressSettings>,
    pub reruns: Option<RerunSettings>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectivePriceComponent {
    Compute,
    Disk,
    Licensing,
    PublicAddress,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EffectivePrice {
    pub id: String,
    pub scope: crate::policy::FinancialPriceScope,
    pub component: EffectivePriceComponent,
    pub currency: String,
    #[serde(
        deserialize_with = "deserialize_quoted_decimal",
        serialize_with = "serialize_quoted_decimal"
    )]
    pub hourly_rate: rust_decimal::Decimal,
    pub source: String,
    pub reference: String,
    pub observed_at_unix_seconds: u64,
    pub valid_from_unix_seconds: u64,
    pub valid_until_unix_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RerunSettings {
    pub enabled: bool,
    pub max_per_run: u32,
    pub actions_write_permission: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HistorySettings {
    pub window_seconds: u64,
    pub sampling_interval_seconds: u64,
    pub maximum_gap_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IngressSettings {
    pub github_api_origin: String,
    pub github_token_file: String,
    pub garm_api_origin: String,
    pub garm_token_file: String,
    pub controller_id: String,
    pub provider_name: String,
    pub pool_flavor: String,
    pub pool_image: String,
    pub runner_prefix: String,
    pub max_dynamic_pools: u32,
    pub max_runners_per_pool: u32,
    pub allocation_timeout_seconds: u64,
    pub bootstrap_timeout_minutes: u32,
    pub timeout_margin_seconds: u64,
    pub pool_ttl_seconds: u64,
    pub webhook_max_bytes: usize,
    pub repositories: BTreeMap<String, IngressRepository>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IngressRepository {
    pub garm_repository_id: String,
    pub webhook_secret_file: String,
    #[serde(default)]
    pub allow_forks: bool,
    pub template_id: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdmissionSettings {
    pub state_file: std::path::PathBuf,
    pub max_window_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ControlSettings {
    pub listen_address: String,
    pub database_path: String,
    pub auth_token_file: String,
    pub provider_timeout_seconds: u64,
}

impl ConfigPatch {
    /// Merge central defaults with a named profile and permitted job overrides.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid versions, forbidden overrides, missing
    /// central settings or profiles, and policy or administrator-limit failures.
    pub fn normalize_profile(
        &self,
        repository: Option<&ConfigPatch>,
        profile_name: &str,
        repository_or_job: Option<&JobOverrides>,
    ) -> Result<ResolvedRequest, ConfigError> {
        let central = self;
        self.validate_version()?;
        self.validate_rerun_settings()?;
        if let Some(repository) = repository {
            repository.validate_version()?;
            if repository.defaults.is_some()
                || repository.limits.is_some()
                || repository.default_purchasing.is_some()
                || repository.default_policy.is_some()
                || repository.policies.is_some()
                || repository.cost_horizon_seconds.is_some()
                || repository.max_price_age_seconds.is_some()
                || repository.history.is_some()
                || repository.control.is_some()
                || repository.admission.is_some()
                || repository.ingress.is_some()
                || repository.provider_bindings.is_some()
                || repository.aws_catalog_bindings.is_some()
                || repository.azure_catalog_bindings.is_some()
                || repository.gcp_catalog_bindings.is_some()
                || repository.effective_prices.is_some()
                || repository.reruns.is_some()
            {
                return Err(ConfigError::ForbiddenOverride(
                    "administrator configuration".to_owned(),
                ));
            }
        }

        validate_profile_overrides(repository_or_job)?;

        let limits = central
            .limits
            .as_ref()
            .ok_or_else(|| ConfigError::InvalidProfile("central limits are required".to_owned()))?;
        let purchasing_default = central.default_purchasing.as_ref().ok_or_else(|| {
            ConfigError::InvalidProfile("central purchasing default is required".to_owned())
        })?;
        let policy_default = central.default_policy.as_ref().ok_or_else(|| {
            ConfigError::InvalidProfile("central default policy is required".to_owned())
        })?;
        let cost_horizon_seconds = central
            .cost_horizon_seconds
            .filter(|seconds| *seconds > 0)
            .ok_or_else(|| {
                ConfigError::InvalidProfile("central cost horizon must be positive".to_owned())
            })?;
        let max_price_age_seconds = central
            .max_price_age_seconds
            .filter(|seconds| *seconds > 0)
            .ok_or_else(|| {
                ConfigError::InvalidProfile("central max price age must be positive".to_owned())
            })?;
        if !limits.allowed_purchasing.contains(purchasing_default) {
            return Err(ConfigError::HardLimit(
                "central purchasing default".to_owned(),
            ));
        }

        let policies = central.policies.as_ref().ok_or_else(|| {
            ConfigError::InvalidProfile("central policy definitions are required".to_owned())
        })?;
        validate_fallback_graph(policies)
            .map_err(|error| ConfigError::InvalidProfile(error.to_string()))?;
        let history_required = policies
            .values()
            .any(|definition| match &definition.strategy {
                crate::policy::Policy::Continuity { .. } => true,
                crate::policy::Policy::Weighted(policy) => {
                    policy.failure_weight > rust_decimal::Decimal::ZERO
                        || policy.interruption_weight > rust_decimal::Decimal::ZERO
                        || policy.latency_weight > rust_decimal::Decimal::ZERO
                }
                crate::policy::Policy::Economy { .. } | crate::policy::Policy::Ordered { .. } => {
                    false
                }
            });
        if let Some(history) = central.history.as_ref()
            && (history.window_seconds == 0
                || history.sampling_interval_seconds == 0
                || history.maximum_gap_seconds == 0
                || history.sampling_interval_seconds > history.maximum_gap_seconds
                || history.maximum_gap_seconds > history.window_seconds)
        {
            return Err(ConfigError::InvalidProfile(
                "central history settings must be positive and bounded".to_owned(),
            ));
        }
        if history_required && central.history.is_none() {
            return Err(ConfigError::InvalidProfile(
                "central history settings are required for historical policies".to_owned(),
            ));
        }
        let central_profile = central
            .profiles
            .as_ref()
            .and_then(|profiles| profiles.get(profile_name));
        let local_profile = repository.and_then(|repository| {
            repository
                .profiles
                .as_ref()
                .and_then(|profiles| profiles.get(profile_name))
        });
        if central_profile.is_none() && local_profile.is_none() {
            return Err(ConfigError::MissingProfile(profile_name.to_owned()));
        }

        let mut merged = central
            .defaults
            .as_ref()
            .ok_or_else(|| ConfigError::InvalidProfile("central defaults are required".to_owned()))?
            .clone();
        if merged.purchasing.is_none() {
            merged.purchasing = Some(purchasing_default.clone());
        }

        if merged.purchasing_models.is_none() {
            merged.purchasing_models = Some([purchasing_default.clone()].into_iter().collect());
        }

        if merged.purchasing_tiers.is_none() {
            merged.purchasing_tiers = Some(vec![vec![purchasing_default.clone()]]);
        }

        if merged.policy.is_none() {
            merged.policy = Some(policy_default.clone());
        }

        if let Some(patch) = central_profile {
            merged.apply(patch);
        }

        if let Some(patch) = local_profile {
            merged.apply(patch);
        }

        if let Some(overrides) = repository_or_job {
            merged.apply(&overrides.profile);
        }

        let profile = merged.finish(cost_horizon_seconds, max_price_age_seconds, limits)?;
        if !policies.contains_key(&profile.policy) {
            return Err(ConfigError::InvalidProfile(format!(
                "policy {} is undefined",
                profile.policy
            )));
        }

        Self::validate_limits(&profile, limits)?;
        Ok(ResolvedRequest {
            profile_name: profile_name.to_owned(),
            profile,
        })
    }

    /// Validate central administrator rerun controls at startup and request normalization.
    ///
    /// # Errors
    /// Returns an error when the bound or Actions write declaration is invalid.
    pub fn validate_rerun_settings(&self) -> Result<(), ConfigError> {
        if self.reruns.as_ref().is_some_and(|reruns| {
            reruns.max_per_run == 0
                || reruns.max_per_run > 100
                || (reruns.enabled && !reruns.actions_write_permission)
        }) {
            return Err(ConfigError::HardLimit("rerun settings".to_owned()));
        }
        Ok(())
    }

    fn validate_limits(
        profile: &RunnerProfile,
        limits: &AdministratorLimits,
    ) -> Result<(), ConfigError> {
        for (name, value, maximum, allow_zero) in [
            ("vcpu_min", profile.vcpu_min, limits.max_vcpu, false),
            ("vcpu_max", profile.vcpu_max, limits.max_vcpu, false),
            (
                "memory_gib_min",
                profile.memory_gib_min,
                limits.max_memory_gib,
                false,
            ),
            (
                "memory_gib_max",
                profile.memory_gib_max,
                limits.max_memory_gib,
                false,
            ),
            (
                "boot_disk.size_gib",
                profile.boot_disk.size_gib,
                limits.max_boot_disk_gib,
                false,
            ),
            (
                "local_scratch_gib",
                profile.local_scratch_gib,
                limits.max_local_scratch_gib,
                true,
            ),
        ] {
            if (!allow_zero && value == 0) || value > maximum {
                return Err(ConfigError::HardLimit(name.to_owned()));
            }
        }

        if profile.vcpu_min > profile.vcpu_max || profile.memory_gib_min > profile.memory_gib_max {
            return Err(ConfigError::InvalidProfile(
                "minimum resource value exceeds maximum".to_owned(),
            ));
        }

        if profile.boot_disk.iops.unwrap_or_default() > limits.max_boot_disk_iops
            || profile.boot_disk.throughput_mib_s.unwrap_or_default()
                > limits.max_boot_disk_throughput_mib_s
        {
            return Err(ConfigError::HardLimit(
                "disk performance or maximum cost".to_owned(),
            ));
        }

        if !limits.allowed_os.contains(&profile.os)
            || !limits.allowed_architectures.contains(&profile.architecture)
            || !limits.allowed_images.contains(&profile.image)
            || !limits.allowed_purchasing.contains(&profile.purchasing)
            || !profile
                .purchasing_models
                .is_subset(&limits.allowed_purchasing)
            || profile
                .accounts
                .iter()
                .any(|account| !limits.allowed_accounts.contains(account))
            || profile
                .locations
                .iter()
                .any(|location| !limits.allowed_locations.contains(location))
            || !profile.capabilities.is_subset(&limits.allowed_capabilities)
            || profile.gpu.as_ref().is_some_and(|gpu| {
                gpu.count > limits.max_gpu_count
                    || gpu.memory_gib.unwrap_or_default() > limits.max_gpu_memory_gib
            })
            || profile.gpu.as_ref().is_some_and(|gpu| {
                !limits.allowed_gpu_vendors.contains(&gpu.vendor)
                    || gpu
                        .model
                        .as_ref()
                        .is_some_and(|model| !limits.allowed_gpu_models.contains(model))
            })
            || profile
                .purchasing_tiers
                .iter()
                .flatten()
                .any(|model| !profile.purchasing_models.contains(model))
        {
            return Err(ConfigError::HardLimit(
                "OS, architecture, image, purchasing, or capability".to_owned(),
            ));
        }

        if limits.max_total_cost.is_some_and(|maximum| {
            profile
                .maximum_total_cost
                .is_some_and(|requested| requested > maximum)
        }) {
            return Err(ConfigError::HardLimit("maximum total cost".to_owned()));
        }

        validate_purchasing_tiers(profile)?;
        validate_machine_allowlists(profile, limits)?;
        Ok(())
    }
}

fn validate_profile_overrides(overrides: Option<&JobOverrides>) -> Result<(), ConfigError> {
    let Some(overrides) = overrides else {
        return Ok(());
    };

    for (name, present) in [
        ("network_id", overrides.network_id.is_some()),
        ("credentials", overrides.credentials.is_some()),
        ("bootstrap_script", overrides.bootstrap_script.is_some()),
    ] {
        if present {
            return Err(ConfigError::ForbiddenOverride(name.to_owned()));
        }
    }

    Ok(())
}

fn validate_purchasing_tiers(profile: &RunnerProfile) -> Result<(), ConfigError> {
    let tiered_models: BTreeSet<String> =
        profile.purchasing_tiers.iter().flatten().cloned().collect();
    if profile.purchasing_tiers.iter().any(Vec::is_empty)
        || profile.purchasing_tiers.iter().flatten().count() != tiered_models.len()
        || tiered_models != profile.purchasing_models
    {
        return Err(ConfigError::InvalidProfile(
            "purchasing tiers must cover each permitted model once".to_owned(),
        ));
    }

    Ok(())
}

fn validate_machine_allowlists(
    profile: &RunnerProfile,
    limits: &AdministratorLimits,
) -> Result<(), ConfigError> {
    for (name, requested, allowed_by_cloud) in [
        (
            "machine_types",
            &profile.machine_types,
            &limits.allowed_machine_types,
        ),
        (
            "machine_families",
            &profile.machine_families,
            &limits.allowed_machine_families,
        ),
    ] {
        for (cloud, values) in requested {
            if !limits.allowed_clouds.contains(cloud)
                || values.iter().any(|value| {
                    !allowed_by_cloud
                        .get(cloud)
                        .is_some_and(|allowed| allowed.contains(value))
                })
            {
                return Err(ConfigError::HardLimit(format!("{name}.{cloud}")));
            }
        }
    }

    Ok(())
}
