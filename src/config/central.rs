use super::catalog_source::{AwsCatalogSource, AzureCatalogSource, GcpCatalogSource};
use super::{
    AdministratorLimits, AdmissionSettings, AwsCatalogBinding, AwsNetworkBinding,
    AzureCatalogBinding, AzureNetworkBinding, ConfigError, ConfigPatch, ControlSettings,
    EffectivePrice, GcpCatalogBinding, HistorySettings, IngressSettings, RerunSettings,
    RunnerProfilePatch,
};
use crate::policy::PolicyDefinition;
use crate::providers::ProviderBinding;
use crate::providers::PurchasingCapability;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CentralConfig {
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
    pub authentication: Option<crate::authentication::TokenRenewalConfig>,
    pub webhook_listen_address: Option<String>,
    pub admission: Option<AdmissionSettings>,
    pub aws_catalog_bindings: Option<Vec<AwsCatalogSource>>,
    pub azure_catalog_bindings: Option<Vec<AzureCatalogSource>>,
    pub gcp_catalog_bindings: Option<Vec<GcpCatalogSource>>,
    pub effective_prices: Option<Vec<EffectivePrice>>,
    pub ingress: Option<IngressSettings>,
    pub reruns: Option<RerunSettings>,
    pub garm: Option<GarmSource>,
    #[serde(default)]
    pub providers: Vec<ProviderSource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GarmSource {
    pub config_file: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSource {
    pub name: String,
    pub cloud: String,
    #[serde(default)]
    pub executable_sha256: String,
    #[serde(default)]
    pub configuration_sha256: String,
    #[serde(default)]
    pub source_revision: String,
    #[serde(default)]
    pub expected_version: String,
    #[serde(default)]
    pub supported_os: BTreeSet<String>,
    #[serde(default)]
    pub supported_architectures: BTreeSet<String>,
    #[serde(default)]
    pub purchasing_capabilities: BTreeSet<PurchasingCapability>,
}

impl CentralConfig {
    /// Parse authored policy and native references without reading native files.
    ///
    /// # Errors
    /// Returns an error for malformed documents, unknown fields or versions.
    pub fn from_toml(toml: &str) -> Result<Self, ConfigError> {
        let document = toml
            .parse::<toml_edit::DocumentMut>()
            .map_err(|error| ConfigError::Toml(error.to_string()))?;
        super::document::reject_unknown_policy_fields(&document)?;
        let source: Self = toml_edit::de::from_document(document)
            .map_err(|error| ConfigError::Toml(error.to_string()))?;
        if source.version != Some(1) {
            return Err(ConfigError::UnsupportedVersion);
        }

        Ok(source)
    }

    /// Read native GARM/provider files and materialize secret-free runtime policy.
    ///
    /// # Errors
    /// Returns fixed native errors or invalid-reference errors for unsupported
    /// credentials, missing catalogs, conflicting scopes and invalid pins.
    pub fn load_native_configuration(
        self,
        environment: &BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    ) -> Result<ConfigPatch, ConfigError> {
        use crate::providers::configuration::{NativeCloudScope, load_native_cloud_scope};
        use crate::providers::load_native_garm_providers;
        let mut imported = BTreeMap::new();
        if let Some(garm) = &self.garm {
            if !garm.config_file.is_absolute() {
                return Err(ConfigError::Native(
                    "native GARM configuration path must be absolute".into(),
                ));
            }

            let names = self
                .providers
                .iter()
                .map(|source| source.name.clone())
                .collect::<Vec<_>>();
            let own_name = if names.is_empty() {
                ""
            } else {
                self.ingress
                    .as_ref()
                    .map(|ingress| ingress.provider_name.as_str())
                    .filter(|name| !name.trim().is_empty())
                    .ok_or_else(|| {
                        ConfigError::Native(
                            "ingress provider_name is required for native imports".into(),
                        )
                    })?
            };
            imported = load_native_garm_providers(&garm.config_file, &names, own_name, environment)
                .map_err(|error| ConfigError::Native(error.to_string()))?;
        } else if !self.providers.is_empty() {
            return Err(ConfigError::Native(
                "native GARM configuration is required for providers".into(),
            ));
        }

        let mut scopes = BTreeMap::new();
        let mut bindings = BTreeMap::new();
        for source in self.providers {
            let registration = imported.remove(&source.name).ok_or_else(|| {
                ConfigError::Native("native provider registration is missing".into())
            })?;
            let scope = load_native_cloud_scope(
                &source.cloud,
                &registration.configuration_path,
                &source.configuration_sha256,
            )
            .map_err(|error| ConfigError::Native(error.into()))?;
            let (account, location) = match &scope {
                NativeCloudScope::Aws { region, .. } => {
                    let catalog = self
                        .aws_catalog_bindings
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .find(|catalog| catalog.provider_binding_id == source.name)
                        .ok_or_else(|| {
                            ConfigError::Native("AWS provider requires a matching catalog".into())
                        })?;
                    (catalog.account_id.clone(), region.clone())
                }
                NativeCloudScope::Azure {
                    subscription_id,
                    location,
                    ..
                } => (subscription_id.clone(), location.clone()),
                NativeCloudScope::Gcp {
                    project_id, zone, ..
                } => (project_id.clone(), zone.clone()),
            };
            let name = source.name.clone();
            let binding = source.into_binding(registration, account, location)?;

            scopes.insert(name.clone(), scope);
            bindings.insert(name, binding);
        }

        let mut used = BTreeSet::new();
        let mut catalog_ids = BTreeSet::new();
        let mut aws = Vec::new();
        for catalog in self.aws_catalog_bindings.unwrap_or_default() {
            let Some(NativeCloudScope::Aws { region, subnet_id }) =
                scopes.get(&catalog.provider_binding_id)
            else {
                return Err(ConfigError::Native(
                    "AWS catalog references a missing or different cloud provider".into(),
                ));
            };
            let provider = &bindings[&catalog.provider_binding_id];
            if catalog.id.trim().is_empty()
                || !catalog_ids.insert(catalog.id.clone())
                || catalog.account_id != provider.account
            {
                return Err(ConfigError::Native(
                    "AWS catalog identity conflicts with its provider".into(),
                ));
            }

            used.insert(catalog.provider_binding_id.clone());
            aws.push(AwsCatalogBinding {
                id: catalog.id,
                provider_binding_id: catalog.provider_binding_id,
                account_id: catalog.account_id,
                region: region.clone(),
                pricing_location: catalog.pricing_location,
                credential_reference: format!("provider-binding:{}:configuration", provider.id),
                locations: catalog
                    .locations
                    .into_iter()
                    .map(|(alias, network)| {
                        (
                            alias,
                            AwsNetworkBinding {
                                availability_zone: network.availability_zone,
                                subnet_id: subnet_id.clone(),
                                security_group_ids: network.security_group_ids,
                            },
                        )
                    })
                    .collect(),
                images: catalog.images,
                storage_classes: catalog.storage_classes,
            });
        }

        let mut azure = Vec::new();
        for catalog in self.azure_catalog_bindings.unwrap_or_default() {
            let Some(NativeCloudScope::Azure {
                subscription_id,
                location,
                vnet_subnet_id,
            }) = scopes.get(&catalog.provider_binding_id)
            else {
                return Err(ConfigError::Native(
                    "Azure catalog references a missing or different cloud provider".into(),
                ));
            };
            if catalog.id.trim().is_empty() || !catalog_ids.insert(catalog.id.clone()) {
                return Err(ConfigError::Native(
                    "Azure catalog identity is invalid or duplicated".into(),
                ));
            }

            used.insert(catalog.provider_binding_id.clone());
            azure.push(AzureCatalogBinding {
                id: catalog.id,
                credential_reference: format!(
                    "provider-binding:{}:configuration",
                    catalog.provider_binding_id
                ),
                provider_binding_id: catalog.provider_binding_id,
                account_id: subscription_id.clone(),
                subscription_id: subscription_id.clone(),
                pricing_location: location.clone(),
                locations: catalog
                    .locations
                    .into_iter()
                    .map(|(alias, network)| {
                        (
                            alias,
                            AzureNetworkBinding {
                                location: location.clone(),
                                vnet_subnet_id: vnet_subnet_id.clone(),
                                public_ip_sku: network.public_ip_sku,
                            },
                        )
                    })
                    .collect(),
                images: catalog.images,
                storage_classes: catalog.storage_classes,
                accelerator_facts: catalog.accelerator_facts,
            });
        }

        let mut gcp = Vec::new();
        for catalog in self.gcp_catalog_bindings.unwrap_or_default() {
            let Some(NativeCloudScope::Gcp {
                project_id,
                zone,
                region,
            }) = scopes.get(&catalog.provider_binding_id)
            else {
                return Err(ConfigError::Native(
                    "GCP catalog references a missing or different cloud provider".into(),
                ));
            };
            if catalog.id.trim().is_empty() || !catalog_ids.insert(catalog.id.clone()) {
                return Err(ConfigError::Native(
                    "GCP catalog identity is invalid or duplicated".into(),
                ));
            }

            used.insert(catalog.provider_binding_id.clone());
            gcp.push(GcpCatalogBinding {
                id: catalog.id,
                credential_reference: format!(
                    "provider-binding:{}:configuration",
                    catalog.provider_binding_id
                ),
                provider_binding_id: catalog.provider_binding_id,
                project_id: project_id.clone(),
                zone: zone.clone(),
                pricing_location: region.clone(),
                images: catalog.images,
                storage_classes: catalog.storage_classes,
                storage_billing_skus: catalog.storage_billing_skus,
                external_ip_billing_sku: catalog.external_ip_billing_sku,
                accelerator_facts: catalog.accelerator_facts,
                machine_facts: catalog.machine_facts,
                disk_performance: catalog.disk_performance,
                capacity_advice_enabled: catalog.capacity_advice_enabled,
            });
        }

        if used.len() != bindings.len() {
            return Err(ConfigError::Native(
                "each selected provider requires a matching cloud catalog".into(),
            ));
        }

        validate_resolved_catalogs(&aws, &azure, &gcp, &bindings)?;

        let patch = ConfigPatch {
            version: self.version,
            profiles: self.profiles,
            defaults: self.defaults,
            limits: self.limits,
            default_purchasing: self.default_purchasing,
            default_policy: self.default_policy,
            policies: self.policies,
            cost_horizon_seconds: self.cost_horizon_seconds,
            max_price_age_seconds: self.max_price_age_seconds,
            history: self.history,
            control: self.control,
            admission: self.admission,
            effective_prices: self.effective_prices,
            ingress: self.ingress,
            reruns: self.reruns,
            provider_bindings: (!bindings.is_empty()).then(|| bindings.into_values().collect()),
            aws_catalog_bindings: (!aws.is_empty()).then_some(aws),
            azure_catalog_bindings: (!azure.is_empty()).then_some(azure),
            gcp_catalog_bindings: (!gcp.is_empty()).then_some(gcp),
        };
        patch.validate_version()?;
        Ok(patch)
    }
}

fn validate_resolved_catalogs(
    aws: &[AwsCatalogBinding],
    azure: &[AzureCatalogBinding],
    gcp: &[GcpCatalogBinding],
    bindings: &BTreeMap<String, ProviderBinding>,
) -> Result<(), ConfigError> {
    for catalog in aws {
        crate::cloud::aws::AwsBackend::settings_from_bindings(
            catalog,
            &bindings[&catalog.provider_binding_id],
            false,
        )
        .map_err(|_| ConfigError::Native("AWS catalog metadata is invalid".into()))?;
    }

    for catalog in azure {
        crate::cloud::azure::AzureBackend::settings_from_bindings(
            catalog,
            &bindings[&catalog.provider_binding_id],
        )
        .map_err(|_| ConfigError::Native("Azure catalog metadata is invalid".into()))?;
    }

    for catalog in gcp {
        crate::cloud::gcp::GcpBackend::settings_from_bindings(
            catalog,
            &bindings[&catalog.provider_binding_id],
        )
        .map_err(|_| ConfigError::Native("GCP catalog metadata is invalid".into()))?;
    }

    Ok(())
}

impl ProviderSource {
    fn into_binding(
        self,
        registration: crate::providers::NativeProviderRegistration,
        account: String,
        location: String,
    ) -> Result<ProviderBinding, ConfigError> {
        let binding = ProviderBinding {
            id: self.name.clone(),
            cloud: self.cloud,
            account,
            location,
            executable_path: registration.executable_path,
            executable_sha256: self.executable_sha256,
            configuration_path: registration.configuration_path,
            configuration_sha256: self.configuration_sha256,
            source_revision: self.source_revision,
            expected_version: self.expected_version,
            inherited_environment: registration.inherited_environment,
            supported_os: self.supported_os,
            supported_architectures: self.supported_architectures,
            purchasing_capabilities: self.purchasing_capabilities,
        };
        if self.name.trim().is_empty() || !crate::providers::valid_binding_metadata(&binding) {
            return Err(ConfigError::Native(
                "native provider qualification metadata is invalid".into(),
            ));
        }

        Ok(binding)
    }
}
