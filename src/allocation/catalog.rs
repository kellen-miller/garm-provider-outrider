use super::{
    AllocationError, AllocationService, Arc, AtomicUsize, AwsBackend, AzureBackend, BTreeMap,
    BTreeSet, CatalogBackend, CatalogLocationScope, CatalogOffer, CatalogQuery, ConfigPatch,
    ConnectedCatalog, Duration, GcpBackend, HashMap, HashSet, Instant, InterruptionEvidence,
    Journal, OsString, OwnershipScope, PlacementIdentity, ProcessLimits, ProviderBinding,
    ProviderPinKey, ProviderRegistry, RunnerProfile, SnapshotDocument, unix_now_nanos,
};
use crate::cloud::CloudCatalog;
use sha2::Digest;

impl ConnectedCatalog {
    pub fn aws(
        backend: AwsBackend,
        configuration_fingerprint: String,
        catalog_id: String,
        binding_id: String,
        account_id: String,
        region: String,
    ) -> Self {
        Self {
            backend: CatalogBackend::Aws(backend),
            configuration_fingerprint,
            catalog_id,
            binding_id,
            cloud: "aws".to_owned(),
            account_id,
            locations: [region].into_iter().collect(),
        }
    }

    pub fn azure(
        backend: AzureBackend,
        configuration_fingerprint: String,
        catalog_id: String,
        binding_id: String,
        subscription_id: String,
        locations: BTreeSet<String>,
    ) -> Self {
        Self {
            backend: CatalogBackend::Azure(backend),
            configuration_fingerprint,
            catalog_id,
            binding_id,
            cloud: "azure".to_owned(),
            account_id: subscription_id,
            locations,
        }
    }

    pub fn gcp(
        backend: GcpBackend,
        configuration_fingerprint: String,
        catalog_id: String,
        binding_id: String,
        project_id: String,
        zone: String,
    ) -> Self {
        Self {
            backend: CatalogBackend::Gcp(backend),
            configuration_fingerprint,
            catalog_id,
            binding_id,
            cloud: "gcp".to_owned(),
            account_id: project_id,
            locations: [zone].into_iter().collect(),
        }
    }

    pub(super) fn cloud(&self) -> &str {
        self.cloud.as_str()
    }

    pub(super) fn requested_locations(
        &self,
        config: &ConfigPatch,
        profile: &RunnerProfile,
    ) -> Vec<CatalogLocationScope> {
        match self.cloud() {
            "aws" => config
                .aws_catalog_bindings
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|binding| {
                    catalog_configuration_fingerprint(config, "aws", &binding.id)
                        .is_ok_and(|value| value == self.configuration_fingerprint)
                })
                .map(|binding| {
                    binding
                        .locations
                        .keys()
                        .filter(|alias| {
                            profile.locations.is_empty() || profile.locations.contains(*alias)
                        })
                        .map(|alias| CatalogLocationScope {
                            configured: alias.clone(),
                            native: binding.region.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            "azure" => config
                .azure_catalog_bindings
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|binding| {
                    catalog_configuration_fingerprint(config, "azure", &binding.id)
                        .is_ok_and(|value| value == self.configuration_fingerprint)
                })
                .map(|binding| {
                    binding
                        .locations
                        .iter()
                        .filter(|(alias, _)| {
                            profile.locations.is_empty() || profile.locations.contains(*alias)
                        })
                        .map(|(alias, location)| CatalogLocationScope {
                            configured: alias.clone(),
                            native: location.location.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            "gcp" => config
                .gcp_catalog_bindings
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|binding| {
                    catalog_configuration_fingerprint(config, "gcp", &binding.id)
                        .is_ok_and(|value| value == self.configuration_fingerprint)
                })
                .filter(|binding| {
                    profile.locations.is_empty()
                        || profile.locations.contains(&binding.pricing_location)
                })
                .map(|binding| {
                    vec![CatalogLocationScope {
                        configured: binding.pricing_location.clone(),
                        native: binding.zone.clone(),
                    }]
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    pub(super) async fn catalog(
        &self,
        profile: RunnerProfile,
        deadline: Instant,
    ) -> Result<crate::cloud::CatalogDiscovery, crate::cloud::CloudError> {
        let query = CatalogQuery {
            clouds: vec![self.cloud().to_owned()],
            accounts: Vec::new(),
            locations: Vec::new(),
            profile,
        };
        match &self.backend {
            CatalogBackend::Aws(adapter) => adapter.catalog(query, deadline).await,
            CatalogBackend::Azure(adapter) => adapter.catalog(query, deadline).await,
            CatalogBackend::Gcp(adapter) => adapter.catalog(query, deadline).await,
        }
    }

    pub(super) async fn quote(
        &self,
        offer: &CatalogOffer,
        duration: Duration,
        maximum_price_age: Duration,
        deadline: Instant,
    ) -> Result<crate::policy::PriceQuote, crate::cloud::CloudError> {
        match &self.backend {
            CatalogBackend::Aws(adapter) => {
                adapter
                    .quote(offer, duration, maximum_price_age, deadline)
                    .await
            }
            CatalogBackend::Azure(adapter) => {
                adapter
                    .quote(offer, duration, maximum_price_age, deadline)
                    .await
            }
            CatalogBackend::Gcp(adapter) => {
                adapter
                    .quote(offer, duration, maximum_price_age, deadline)
                    .await
            }
        }
    }

    pub(super) async fn inspect(
        &self,
        identity: &PlacementIdentity,
        deadline: Instant,
    ) -> Result<Option<crate::cloud::ProviderInstance>, crate::cloud::CloudError> {
        if identity.cloud != self.cloud() {
            return Ok(None);
        }
        match &self.backend {
            CatalogBackend::Aws(adapter) => Box::pin(adapter.inspect(identity, deadline)).await,
            CatalogBackend::Azure(adapter) => adapter.inspect(identity, deadline).await,
            CatalogBackend::Gcp(adapter) => adapter.inspect(identity, deadline).await,
        }
    }

    pub(super) async fn list(
        &self,
        scope: &OwnershipScope,
        deadline: Instant,
    ) -> Result<Vec<crate::cloud::ProviderInstance>, crate::cloud::CloudError> {
        match &self.backend {
            CatalogBackend::Aws(adapter) => adapter.list(scope, deadline).await,
            CatalogBackend::Azure(adapter) => adapter.list(scope, deadline).await,
            CatalogBackend::Gcp(adapter) => adapter.list(scope, deadline).await,
        }
    }

    pub(super) async fn observe_interruption(
        &self,
        identity: &PlacementIdentity,
        deadline: Instant,
    ) -> Result<Vec<InterruptionEvidence>, crate::cloud::CloudError> {
        match &self.backend {
            CatalogBackend::Aws(adapter) => adapter.observe_interruption(identity, deadline).await,
            CatalogBackend::Azure(adapter) => {
                adapter.observe_interruption(identity, deadline).await
            }
            CatalogBackend::Gcp(adapter) => adapter.observe_interruption(identity, deadline).await,
        }
    }

    pub(super) fn supports_identity(&self, identity: &PlacementIdentity) -> bool {
        let account_matches = if self.cloud == "azure" {
            self.account_id.eq_ignore_ascii_case(&identity.account_id)
        } else {
            self.account_id == identity.account_id
        };
        let location_matches = self.locations.iter().any(|location| {
            if self.cloud == "azure" {
                location.eq_ignore_ascii_case(&identity.location)
            } else {
                location == &identity.location
            }
        });
        self.cloud == identity.cloud && account_matches && location_matches
    }
}

pub(super) fn binding_pin_key(binding: &ProviderBinding) -> ProviderPinKey {
    ProviderPinKey {
        binding_id: binding.id.clone(),
        source_revision: binding.source_revision.clone(),
        executable_sha256: binding.executable_sha256.clone(),
        configuration_sha256: binding.configuration_sha256.clone(),
    }
}
impl AllocationService {
    /// Connect the read-only cloud catalogs and validate the pinned child registry.
    ///
    /// # Errors
    /// Returns an error if required administrator limits, bindings, credentials,
    /// or provider pins are unavailable or inconsistent.
    pub async fn connect(
        config: ConfigPatch,
        journal: Arc<Journal>,
    ) -> Result<Self, AllocationError> {
        let limits = config
            .limits
            .clone()
            .ok_or(AllocationError::Configuration)?;
        if config
            .control
            .as_ref()
            .is_none_or(|settings| settings.provider_timeout_seconds == 0)
        {
            return Err(AllocationError::Configuration);
        }
        if limits.max_allocations_controller == 0
            || limits.max_allocations_account == 0
            || limits.max_allocations_pool == 0
        {
            return Err(AllocationError::Configuration);
        }

        let bindings = config
            .provider_bindings
            .clone()
            .ok_or(AllocationError::Configuration)?;
        if bindings.is_empty() {
            return Err(AllocationError::Configuration);
        }
        let process_environment = std::env::vars_os().collect::<BTreeMap<_, _>>();
        let provider_registry = ProviderRegistry::new(
            bindings,
            process_environment.clone(),
            ProcessLimits::default(),
        )
        .map_err(|_| AllocationError::Configuration)?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut catalogs = Vec::new();
        let retained_configs = retained_catalog_configs(&journal, &config)?;
        let mut connected_fingerprints = HashSet::new();
        for retained_config in &retained_configs {
            let required_fingerprints = &retained_config.required_fingerprints;
            let retained_config = &retained_config.config;
            let retained_bindings = retained_config
                .provider_bindings
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|binding| (binding.id.as_str(), binding))
                .collect::<HashMap<_, _>>();
            for catalog in retained_config
                .aws_catalog_bindings
                .as_deref()
                .unwrap_or_default()
            {
                let fingerprint =
                    match catalog_configuration_fingerprint(retained_config, "aws", &catalog.id) {
                        Ok(fingerprint) => fingerprint,
                        Err(_) if required_fingerprints.is_some() => continue,
                        Err(error) => return Err(error),
                    };
                if required_fingerprints
                    .as_ref()
                    .is_some_and(|required| !required.contains(&fingerprint))
                {
                    continue;
                }

                if !connected_fingerprints.insert(fingerprint.clone()) {
                    continue;
                }
                let binding = retained_bindings
                    .get(catalog.provider_binding_id.as_str())
                    .ok_or(AllocationError::Configuration)?;
                let settings = AwsBackend::settings_from_bindings(catalog, binding, false)
                    .map_err(|_| AllocationError::Configuration)?;
                let backend = AwsBackend::connect(settings, deadline)
                    .await
                    .map_err(|_| AllocationError::Catalog)?;
                catalogs.push(ConnectedCatalog::aws(
                    backend,
                    fingerprint,
                    catalog.id.clone(),
                    catalog.provider_binding_id.clone(),
                    catalog.account_id.clone(),
                    catalog.region.clone(),
                ));
            }
            for catalog in retained_config
                .azure_catalog_bindings
                .as_deref()
                .unwrap_or_default()
            {
                let fingerprint = match catalog_configuration_fingerprint(
                    retained_config,
                    "azure",
                    &catalog.id,
                ) {
                    Ok(fingerprint) => fingerprint,
                    Err(_) if required_fingerprints.is_some() => continue,
                    Err(error) => return Err(error),
                };
                if required_fingerprints
                    .as_ref()
                    .is_some_and(|required| !required.contains(&fingerprint))
                {
                    continue;
                }

                if !connected_fingerprints.insert(fingerprint.clone()) {
                    continue;
                }
                let binding = retained_bindings
                    .get(catalog.provider_binding_id.as_str())
                    .ok_or(AllocationError::Configuration)?;
                let settings = AzureBackend::settings_from_bindings(catalog, binding)
                    .map_err(|_| AllocationError::Configuration)?;
                let backend = AzureBackend::connect(settings, deadline)
                    .await
                    .map_err(|_| AllocationError::Catalog)?;
                let locations = catalog
                    .locations
                    .values()
                    .map(|location| location.location.clone())
                    .collect();
                catalogs.push(ConnectedCatalog::azure(
                    backend,
                    fingerprint,
                    catalog.id.clone(),
                    catalog.provider_binding_id.clone(),
                    catalog.subscription_id.clone(),
                    locations,
                ));
            }
            for catalog in retained_config
                .gcp_catalog_bindings
                .as_deref()
                .unwrap_or_default()
            {
                let fingerprint =
                    match catalog_configuration_fingerprint(retained_config, "gcp", &catalog.id) {
                        Ok(fingerprint) => fingerprint,
                        Err(_) if required_fingerprints.is_some() => continue,
                        Err(error) => return Err(error),
                    };
                if required_fingerprints
                    .as_ref()
                    .is_some_and(|required| !required.contains(&fingerprint))
                {
                    continue;
                }

                if !connected_fingerprints.insert(fingerprint.clone()) {
                    continue;
                }
                let binding = retained_bindings
                    .get(catalog.provider_binding_id.as_str())
                    .ok_or(AllocationError::Configuration)?;
                let settings = GcpBackend::settings_from_bindings(catalog, binding)
                    .map_err(|_| AllocationError::Configuration)?;
                let backend = GcpBackend::connect(settings, deadline)
                    .await
                    .map_err(|_| AllocationError::Catalog)?;
                catalogs.push(ConnectedCatalog::gcp(
                    backend,
                    fingerprint,
                    catalog.id.clone(),
                    catalog.provider_binding_id.clone(),
                    catalog.project_id.clone(),
                    catalog.zone.clone(),
                ));
            }
        }
        for retained_config in &retained_configs {
            if retained_config
                .required_fingerprints
                .as_ref()
                .is_some_and(|required| {
                    required
                        .iter()
                        .any(|fingerprint| !connected_fingerprints.contains(fingerprint))
                })
            {
                return Err(AllocationError::Configuration);
            }
        }
        if catalogs.is_empty() {
            return Err(AllocationError::Configuration);
        }

        Self::new(
            config,
            journal,
            catalogs,
            provider_registry,
            process_environment,
        )
    }

    /// Construct the production allocation executor from connected read-only catalogs
    /// and an already validated pinned child registry.
    ///
    /// # Errors
    /// Returns an error when required caps, timeouts, catalog scopes, or child
    /// bindings do not match the administrator configuration.
    pub fn new(
        config: ConfigPatch,
        journal: Arc<Journal>,
        catalogs: Vec<ConnectedCatalog>,
        provider_registry: ProviderRegistry,
        process_environment: BTreeMap<OsString, OsString>,
    ) -> Result<Self, AllocationError> {
        let limits = config
            .limits
            .clone()
            .ok_or(AllocationError::Configuration)?;
        if config
            .control
            .as_ref()
            .is_none_or(|settings| settings.provider_timeout_seconds == 0)
            || limits.max_allocations_controller == 0
            || limits.max_allocations_account == 0
            || limits.max_allocations_pool == 0
            || catalogs.is_empty()
        {
            return Err(AllocationError::Configuration);
        }
        let bindings = config
            .provider_bindings
            .clone()
            .filter(|bindings| !bindings.is_empty())
            .ok_or(AllocationError::Configuration)?;
        let provider_bindings = bindings
            .into_iter()
            .map(|binding| (binding.id.clone(), binding))
            .collect::<HashMap<_, _>>();
        let configured_fingerprints = configured_catalog_fingerprints(&config)?;
        let connected_fingerprints = catalogs
            .iter()
            .map(|catalog| catalog.configuration_fingerprint.as_str())
            .collect::<HashSet<_>>();
        if connected_fingerprints.len() != catalogs.len()
            || !configured_fingerprints
                .iter()
                .all(|fingerprint| connected_fingerprints.contains(fingerprint.as_str()))
            || catalogs.iter().any(|catalog| {
                configured_fingerprints.contains(&catalog.configuration_fingerprint)
                    && (!provider_bindings.contains_key(&catalog.binding_id)
                        || !catalog_matches_configuration(catalog, &config))
            })
        {
            return Err(AllocationError::Configuration);
        }
        Ok(Self {
            journal,
            config,
            limits,
            catalogs,
            provider_registry,
            provider_bindings,
            process_environment,
            history_session_id: format!("{}-{}", std::process::id(), unix_now_nanos()),
            recovery_offset: AtomicUsize::new(0),
            lifecycle_recovery_offset: AtomicUsize::new(0),
            interruption_recovery_offset: AtomicUsize::new(0),
        })
    }
}

fn retained_catalog_configs(
    journal: &Journal,
    current: &ConfigPatch,
) -> Result<Vec<RetainedCatalogConfig>, AllocationError> {
    let active_snapshots = journal
        .active_pool_snapshots()
        .map_err(|_| AllocationError::Journal)?;
    let mut active_configs = BTreeMap::new();
    let mut configs = vec![RetainedCatalogConfig {
        config: current.clone(),
        required_fingerprints: None,
    }];
    for snapshot in active_snapshots {
        let document = serde_json::from_str::<SnapshotDocument>(&snapshot.config_json)
            .map_err(|_| AllocationError::Snapshot)?;
        active_configs.insert(snapshot.id, document.central.clone());
        configs.push(RetainedCatalogConfig {
            config: document.central,
            required_fingerprints: None,
        });
    }

    let allocations = journal
        .allocations_requiring_retained_config()
        .map_err(|_| AllocationError::Journal)?;
    let mut required_by_snapshot = BTreeMap::<String, BTreeSet<String>>::new();
    for allocation in allocations {
        let fingerprint = allocation
            .catalog_fingerprint
            .ok_or(AllocationError::Configuration)?;
        if let Some(active_config) = active_configs.get(&allocation.snapshot_id) {
            if !catalog_fingerprint_is_configured(active_config, &fingerprint) {
                return Err(AllocationError::Configuration);
            }
        } else {
            required_by_snapshot
                .entry(allocation.snapshot_id)
                .or_default()
                .insert(fingerprint);
        }
    }

    for (snapshot_id, required_fingerprints) in required_by_snapshot {
        let Some(snapshot) = journal
            .pool_snapshot_by_id(&snapshot_id)
            .map_err(|_| AllocationError::Journal)?
        else {
            return Err(AllocationError::Snapshot);
        };
        let document = serde_json::from_str::<SnapshotDocument>(&snapshot.config_json)
            .map_err(|_| AllocationError::Snapshot)?;
        if required_fingerprints
            .iter()
            .any(|fingerprint| !catalog_fingerprint_is_configured(&document.central, fingerprint))
        {
            return Err(AllocationError::Configuration);
        }
        configs.push(RetainedCatalogConfig {
            config: document.central,
            required_fingerprints: Some(required_fingerprints),
        });
    }
    Ok(configs)
}
pub(super) fn catalog_fingerprint_is_configured(config: &ConfigPatch, expected: &str) -> bool {
    for catalog in config.aws_catalog_bindings.as_deref().unwrap_or_default() {
        if catalog_configuration_fingerprint(config, "aws", &catalog.id)
            .is_ok_and(|fingerprint| fingerprint == expected)
        {
            return true;
        }
    }

    for catalog in config.azure_catalog_bindings.as_deref().unwrap_or_default() {
        if catalog_configuration_fingerprint(config, "azure", &catalog.id)
            .is_ok_and(|fingerprint| fingerprint == expected)
        {
            return true;
        }
    }

    for catalog in config.gcp_catalog_bindings.as_deref().unwrap_or_default() {
        if catalog_configuration_fingerprint(config, "gcp", &catalog.id)
            .is_ok_and(|fingerprint| fingerprint == expected)
        {
            return true;
        }
    }

    false
}
pub(super) fn configured_catalog_fingerprints(
    config: &ConfigPatch,
) -> Result<BTreeSet<String>, AllocationError> {
    let mut fingerprints = BTreeSet::new();
    for catalog in config.aws_catalog_bindings.as_deref().unwrap_or_default() {
        fingerprints.insert(catalog_configuration_fingerprint(
            config,
            "aws",
            &catalog.id,
        )?);
    }
    for catalog in config.azure_catalog_bindings.as_deref().unwrap_or_default() {
        fingerprints.insert(catalog_configuration_fingerprint(
            config,
            "azure",
            &catalog.id,
        )?);
    }
    for catalog in config.gcp_catalog_bindings.as_deref().unwrap_or_default() {
        fingerprints.insert(catalog_configuration_fingerprint(
            config,
            "gcp",
            &catalog.id,
        )?);
    }
    Ok(fingerprints)
}
/// Fingerprint one immutable catalog/provider binding pair retained by a snapshot.
///
/// # Errors
/// Returns an error when the selected catalog or its child binding is missing.
pub fn catalog_configuration_fingerprint(
    config: &ConfigPatch,
    cloud: &str,
    catalog_id: &str,
) -> Result<String, AllocationError> {
    let (binding_id, catalog) = match cloud {
        "aws" => {
            let catalog = config
                .aws_catalog_bindings
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|catalog| catalog.id == catalog_id)
                .ok_or(AllocationError::Configuration)?;
            (
                catalog.provider_binding_id.as_str(),
                serde_json::to_value(catalog),
            )
        }
        "azure" => {
            let catalog = config
                .azure_catalog_bindings
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|catalog| catalog.id == catalog_id)
                .ok_or(AllocationError::Configuration)?;
            (
                catalog.provider_binding_id.as_str(),
                serde_json::to_value(catalog),
            )
        }
        "gcp" => {
            let catalog = config
                .gcp_catalog_bindings
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|catalog| catalog.id == catalog_id)
                .ok_or(AllocationError::Configuration)?;
            (
                catalog.provider_binding_id.as_str(),
                serde_json::to_value(catalog),
            )
        }
        _ => return Err(AllocationError::Configuration),
    };
    let catalog = catalog.map_err(|_| AllocationError::Configuration)?;
    let binding = config
        .provider_bindings
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|binding| binding.id == binding_id)
        .ok_or(AllocationError::Configuration)?;
    let bytes = serde_json::to_vec(&serde_json::json!({
        "cloud": cloud,
        "catalog": catalog,
        "provider": binding,
    }))
    .map_err(|_| AllocationError::Configuration)?;
    let digest = sha2::Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}
pub(super) fn catalog_matches_configuration(
    catalog: &ConnectedCatalog,
    config: &ConfigPatch,
) -> bool {
    match catalog.cloud.as_str() {
        "aws" => config
            .aws_catalog_bindings
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|binding| {
                binding.id == catalog.catalog_id
                    && binding.provider_binding_id == catalog.binding_id
                    && binding.account_id == catalog.account_id
                    && catalog.locations == BTreeSet::from([binding.region.clone()])
                    && catalog_configuration_fingerprint(config, "aws", &binding.id)
                        .is_ok_and(|fingerprint| fingerprint == catalog.configuration_fingerprint)
            }),
        "azure" => config
            .azure_catalog_bindings
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|binding| {
                let configured_locations = binding
                    .locations
                    .values()
                    .map(|location| location.location.to_ascii_lowercase())
                    .collect::<BTreeSet<_>>();
                let connected_locations = catalog
                    .locations
                    .iter()
                    .map(|location| location.to_ascii_lowercase())
                    .collect::<BTreeSet<_>>();
                binding.id == catalog.catalog_id
                    && binding.provider_binding_id == catalog.binding_id
                    && binding
                        .subscription_id
                        .eq_ignore_ascii_case(&catalog.account_id)
                    && configured_locations == connected_locations
                    && catalog_configuration_fingerprint(config, "azure", &binding.id)
                        .is_ok_and(|fingerprint| fingerprint == catalog.configuration_fingerprint)
            }),
        "gcp" => config
            .gcp_catalog_bindings
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|binding| {
                binding.id == catalog.catalog_id
                    && binding.provider_binding_id == catalog.binding_id
                    && binding.project_id == catalog.account_id
                    && catalog.locations == BTreeSet::from([binding.zone.clone()])
                    && catalog_configuration_fingerprint(config, "gcp", &binding.id)
                        .is_ok_and(|fingerprint| fingerprint == catalog.configuration_fingerprint)
            }),
        _ => false,
    }
}
struct RetainedCatalogConfig {
    config: ConfigPatch,
    required_fingerprints: Option<BTreeSet<String>>,
}
