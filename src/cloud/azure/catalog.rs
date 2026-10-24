use super::billing::{match_prices, quote_prices, requested_disk_performance};
use super::discovery::{capability_u32, shape_matches_profile, sku_in_scope, sku_shape};
use super::identity::{provider_instance, valid_vm_resource_id};
use super::{
    AzureBackend, AzureLaunchFacts, AzurePriceRequest, AzureVm, AzureVmPage, BTreeMap, BTreeSet,
    COMPUTE_API, Candidate, CatalogOffer, CatalogQuery, CloudCatalog, CloudError, Duration,
    Instant, MAX_OS_DISK_GIB, OwnershipScope, PlacementIdentity, PriceQuote, ProviderInstance,
    ProviderLaunchFacts, ProviderSelection, failure,
};

#[async_trait::async_trait]
impl CloudCatalog for AzureBackend {
    async fn catalog(
        &self,
        query: CatalogQuery,
        deadline: Instant,
    ) -> Result<crate::cloud::CatalogDiscovery, CloudError> {
        if !self.query_matches_scope(&query) {
            return Ok(crate::cloud::CatalogDiscovery::default());
        }
        let profile = query.profile;
        if self.child.use_ephemeral_storage {
            return Err(failure(
                "unsupported",
                "Azure ephemeral OS disks cannot be quoted as managed boot disks",
            ));
        }
        let image = self.configured_image(&profile.image)?;
        let requested_locations = if query.locations.is_empty() {
            self.settings.locations.keys().cloned().collect()
        } else {
            query.locations
        };
        let mut offers = Vec::new();
        let mut exclusion_counts = super::super::CatalogExclusions::default();
        let mut queried = false;
        for alias in requested_locations {
            if !profile.locations.is_empty() && !profile.locations.contains(&alias) {
                continue;
            }
            let Some(location) = self.settings.locations.get(&alias) else {
                continue;
            };
            queried = true;
            let (image_version, image_os, image_arch) = self
                .image_facts(image, &location.location, deadline)
                .await?;
            if image_os != profile.os || image_arch != profile.architecture {
                exclusion_counts.record("image OS or architecture does not match the request");
                continue;
            }
            let inventory = self.sku_inventory(&location.location, deadline).await?;
            let mut shapes = Vec::new();
            for sku in inventory {
                if let Err(reason) = sku_in_scope(&sku, &location.location, &profile) {
                    exclusion_counts.record(reason);
                    continue;
                }
                let Some(shape) = sku_shape(sku) else {
                    exclusion_counts.record("SKU lacks supported machine capability facts");
                    continue;
                };
                if shape.cpu_architecture.as_deref() != Some(image_arch.as_str()) {
                    exclusion_counts.record("SKU CPU architecture does not match the image");
                    continue;
                }
                let Some(machine_family) = shape.sku.family.clone() else {
                    exclusion_counts.record("SKU has no machine family");
                    continue;
                };
                if profile
                    .machine_families
                    .get("azure")
                    .is_some_and(|families| !families.contains(&machine_family))
                    || profile
                        .administrator_limits
                        .allowed_machine_families
                        .get("azure")
                        .is_some_and(|families| !families.contains(&machine_family))
                {
                    exclusion_counts.record("SKU machine family is outside the allowed families");
                    continue;
                }
                let accelerator = self.settings.accelerator_facts.get(&shape.sku.name);
                if !shape_matches_profile(&shape, &profile, accelerator) {
                    exclusion_counts.record("SKU does not meet requested compute capabilities");
                    continue;
                }
                shapes.push((shape, machine_family));
            }

            if shapes.is_empty() {
                continue;
            }

            let machine_types = shapes
                .iter()
                .map(|(shape, _)| shape.sku.name.clone())
                .collect::<Vec<_>>();
            let snapshot = self
                .retail_snapshot(
                    &location.location,
                    &machine_types,
                    Duration::from_secs(profile.max_price_age_seconds),
                    deadline,
                )
                .await?;
            let meters = snapshot.meters.as_ref();
            for (shape, machine_family) in shapes {
                let accelerator = self.settings.accelerator_facts.get(&shape.sku.name);
                for purchasing in profile.purchasing_models.iter().filter(|model| {
                    matches!(model.as_str(), "spot" | "on_demand")
                        && profile
                            .administrator_limits
                            .allowed_purchasing
                            .contains(*model)
                }) {
                    let disk_sku = self
                        .settings
                        .storage_classes
                        .get(&profile.boot_disk.class)
                        .filter(|sku| {
                            matches!(
                                sku.as_str(),
                                "Standard_LRS" | "StandardSSD_LRS" | "Premium_LRS"
                            )
                        });
                    let Some(disk_sku) = disk_sku else {
                        exclusion_counts.record("boot disk storage class is unsupported");
                        continue;
                    };
                    let Some((size, iops, throughput)) =
                        requested_disk_performance(&profile, &shape, image, disk_sku)
                    else {
                        exclusion_counts
                            .record("boot disk does not meet requested performance or size");
                        continue;
                    };
                    let public_ip_sku = location.public_ip_sku.as_deref();
                    let Some(prices) = match_prices(
                        meters,
                        AzurePriceRequest {
                            sku: &shape.sku.name,
                            location: &location.location,
                            os: &profile.os,
                            purchasing,
                            disk_sku,
                            disk_gib: size,
                            licensing: image.licensing,
                            licensing_source: &image.licensing_source,
                            public_requested: public_ip_sku.is_some(),
                        },
                    ) else {
                        exclusion_counts
                            .record("required Azure price meters are missing or ambiguous");
                        continue;
                    };
                    let price = quote_prices(
                        &prices,
                        public_ip_sku.is_some(),
                        snapshot.observed_at_unix_seconds,
                    );
                    let candidate = Candidate {
                        identity: format!(
                            "azure|{}|{}|{}|{}|{}|{}",
                            self.settings.catalog_id,
                            self.settings.subscription_id,
                            alias,
                            shape.sku.name,
                            image.image_id,
                            purchasing
                        ),
                        os: image_os.clone(),
                        architecture: image_arch.clone(),
                        image: profile.image.clone(),
                        vcpu: shape.vcpu,
                        memory_gib: shape.memory_gib,
                        machine_type: shape.sku.name.clone(),
                        machine_family: Some(machine_family.clone()),
                        cloud: "azure".to_owned(),
                        purchasing: purchasing.clone(),
                        account: self.settings.account_id.clone(),
                        location: alias.clone(),
                        gpu_vendor: accelerator.map(|facts| facts.vendor.to_ascii_lowercase()),
                        gpu_model: accelerator.map(|facts| facts.model.clone()),
                        gpu_count: shape.gpu_count,
                        gpu_memory_gib: accelerator.map(|facts| facts.total_memory_gib),
                        boot_disk_class: profile.boot_disk.class.clone(),
                        boot_disk_gib: size,
                        boot_disk_iops: iops,
                        boot_disk_throughput_mib_s: throughput,
                        local_scratch_gib: shape.local_scratch_gib,
                        capabilities: image.capabilities.iter().cloned().collect(),
                        price: Some(price),
                        interruption_rate_per_hour: None,
                        allocation_failure_rate: None,
                        mean_create_seconds: None,
                        observation_age_seconds: None,
                        observation_ages: None,
                        observation_window: None,
                        observation_samples: 0,
                        definitive_create_outcomes: 0,
                        measured_latency_samples: 0,
                        interruption_samples: 0,
                        native_capacity_hint: None,
                    };
                    offers.push(CatalogOffer {
                        candidate,
                        selection: ProviderSelection {
                            binding_id: self.settings.binding_id.clone(),
                            source_revision: self.settings.source_revision.clone(),
                            binary_sha256: self.settings.binary_sha256.clone(),
                            configuration_sha256: self.settings.configuration_sha256.clone(),
                            flavor: shape.sku.name.clone(),
                            image: image.image_id.clone(),
                            approved_extras: BTreeMap::from([
                                ("disk_size_gb".to_owned(), serde_json::json!(size)),
                                (
                                    "storage_account_type".to_owned(),
                                    serde_json::json!(disk_sku),
                                ),
                                (
                                    "allocate_public_ip".to_owned(),
                                    serde_json::json!(public_ip_sku.is_some()),
                                ),
                                ("spot".to_owned(), serde_json::json!(purchasing == "spot")),
                                ("use_ephemeral_storage".to_owned(), serde_json::json!(false)),
                            ]),
                        },
                        launch_facts: ProviderLaunchFacts::Azure(AzureLaunchFacts {
                            subscription_id: self.settings.subscription_id.clone(),
                            location: location.location.clone(),
                            image_version: image_version.clone(),
                            os_disk_sku: disk_sku.to_owned(),
                            public_ip_sku: public_ip_sku.map(str::to_owned),
                            minimum_os_disk_gib: image.minimum_os_disk_gib,
                            minimum_os_disk_source: image.minimum_os_disk_source.clone(),
                            image_licensing: image.licensing,
                            image_licensing_source: image.licensing_source.clone(),
                            maximum_data_disk_count: capability_u32(&shape.sku, "MaxDataDiskCount"),
                            maximum_os_disk_iops: shape.max_disk_iops,
                            maximum_os_disk_throughput_mib_s: shape.max_disk_throughput_mib_s,
                            vm_size_capabilities: shape
                                .sku
                                .capabilities
                                .iter()
                                .map(|cap| (cap.name.clone(), cap.value.clone()))
                                .collect(),
                            ephemeral_os_disk_supported: shape.ephemeral_disk,
                        }),
                    });
                }
            }
        }
        Ok(crate::cloud::CatalogDiscovery {
            offers,
            exclusions: exclusion_counts.into_catalog_exclusions(),
            queried,
        })
    }

    async fn quote(
        &self,
        offer: &CatalogOffer,
        _duration: Duration,
        maximum_price_age: Duration,
        deadline: Instant,
    ) -> Result<PriceQuote, CloudError> {
        let ProviderLaunchFacts::Azure(facts) = &offer.launch_facts else {
            return Err(failure(
                "configuration",
                "Azure quote received non-Azure launch facts",
            ));
        };
        let candidate = &offer.candidate;
        let image = self.configured_image(&candidate.image)?;
        let Some(location) = self.settings.locations.get(&candidate.location) else {
            return Err(failure(
                "configuration",
                "Azure quote location is not configured",
            ));
        };
        let expected_extras = BTreeMap::from([
            (
                "disk_size_gb".to_owned(),
                serde_json::json!(candidate.boot_disk_gib),
            ),
            (
                "storage_account_type".to_owned(),
                serde_json::json!(facts.os_disk_sku),
            ),
            (
                "allocate_public_ip".to_owned(),
                serde_json::json!(facts.public_ip_sku.is_some()),
            ),
            (
                "spot".to_owned(),
                serde_json::json!(candidate.purchasing == "spot"),
            ),
            ("use_ephemeral_storage".to_owned(), serde_json::json!(false)),
        ]);
        if offer.selection.binding_id != self.settings.binding_id
            || offer.selection.source_revision != self.settings.source_revision
            || offer.selection.binary_sha256 != self.settings.binary_sha256
            || offer.selection.configuration_sha256 != self.settings.configuration_sha256
            || offer.selection.approved_extras != expected_extras
            || facts.subscription_id != self.settings.subscription_id
            || facts.location != location.location
            || facts.minimum_os_disk_gib != image.minimum_os_disk_gib
            || facts.minimum_os_disk_source != image.minimum_os_disk_source
            || facts.image_licensing != image.licensing
            || facts.image_licensing_source != image.licensing_source
            || facts.os_disk_sku
                != *self
                    .settings
                    .storage_classes
                    .get(&candidate.boot_disk_class)
                    .ok_or_else(|| {
                        failure("configuration", "Azure quote disk class is not configured")
                    })?
            || facts.public_ip_sku != location.public_ip_sku
            || candidate.account != self.settings.account_id
            || candidate.cloud != "azure"
            || candidate.machine_type != offer.selection.flavor
            || image.image_id != offer.selection.image
            || candidate.boot_disk_gib < image.minimum_os_disk_gib
            || candidate.boot_disk_gib > MAX_OS_DISK_GIB
            || !matches!(candidate.purchasing.as_str(), "spot" | "on_demand")
        {
            return Err(failure(
                "configuration",
                "Azure offer no longer matches its pinned binding",
            ));
        }
        let (image_version, image_os, image_arch) = self
            .image_facts(image, &location.location, deadline)
            .await?;
        if facts.image_version != image_version
            || image_os != candidate.os
            || image_arch != candidate.architecture
        {
            return Err(failure(
                "configuration",
                "Azure image facts no longer match the selected image",
            ));
        }
        let snapshot = self
            .retail_snapshot(
                &location.location,
                std::slice::from_ref(&candidate.machine_type),
                maximum_price_age,
                deadline,
            )
            .await
            .map_err(CloudError::price_unavailable)?;
        let prices = match_prices(
            snapshot.meters.as_ref(),
            AzurePriceRequest {
                sku: &candidate.machine_type,
                location: &location.location,
                os: &candidate.os,
                purchasing: &candidate.purchasing,
                disk_sku: &facts.os_disk_sku,
                disk_gib: candidate.boot_disk_gib,
                licensing: image.licensing,
                licensing_source: &image.licensing_source,
                public_requested: facts.public_ip_sku.is_some(),
            },
        )
        .ok_or_else(|| {
            CloudError::price_unavailable(failure(
                "price",
                "Azure has no unique current matching compute meter",
            ))
        })?;
        if facts.public_ip_sku.is_some() && prices.public_ip.is_none() {
            return Err(CloudError::price_unavailable(failure(
                "price",
                "Azure disk or public IP meter is missing",
            )));
        }
        Ok(quote_prices(
            &prices,
            facts.public_ip_sku.is_some(),
            snapshot.observed_at_unix_seconds,
        ))
    }

    async fn inspect(
        &self,
        identity: &PlacementIdentity,
        deadline: Instant,
    ) -> Result<Option<ProviderInstance>, CloudError> {
        if !identity.cloud.eq_ignore_ascii_case("azure")
            || !identity
                .account_id
                .eq_ignore_ascii_case(&self.settings.subscription_id)
            || !self
                .settings
                .locations
                .values()
                .any(|location| location.location.eq_ignore_ascii_case(&identity.location))
            || !valid_vm_resource_id(&identity.resource_id, &self.settings.subscription_id)
        {
            return Err(failure(
                "configuration",
                "Azure inspection identity does not match the active subscription binding",
            ));
        }
        let url = format!(
            "{}{}?api-version={}&$expand=instanceView",
            self.settings.management_endpoint.trim_end_matches('/'),
            identity.resource_id,
            COMPUTE_API
        );
        let value = self
            .get_management_optional(&url, &identity.resource_id, deadline)
            .await?;
        value
            .map(|value| {
                let vm: AzureVm = serde_json::from_value(value)
                    .map_err(|_| failure("protocol", "Azure VM response was invalid"))?;
                if !vm.id.eq_ignore_ascii_case(&identity.resource_id)
                    || !vm.location.eq_ignore_ascii_case(&identity.location)
                    || vm.tags.get("garm-controller-id") != Some(&identity.controller_id)
                    || identity
                        .dependency_ids
                        .get("vm_id")
                        .is_some_and(|vm_id| vm.properties.vm_id.as_ref() != Some(vm_id))
                {
                    return Err(failure(
                        "configuration",
                        "Azure VM response identity did not match the requested resource",
                    ));
                }
                Ok(provider_instance(vm, &self.settings))
            })
            .transpose()
    }

    async fn list(
        &self,
        scope: &OwnershipScope,
        deadline: Instant,
    ) -> Result<Vec<ProviderInstance>, CloudError> {
        if scope.controller_id.is_empty() {
            return Err(failure(
                "configuration",
                "Azure inventory requires a controller identity",
            ));
        }

        // Ownership needs full VM metadata; statusOnly can omit its tags.
        let url = format!(
            "{}/subscriptions/{}/providers/Microsoft.Compute/virtualMachines?api-version={}",
            self.settings.management_endpoint.trim_end_matches('/'),
            self.settings.subscription_id,
            COMPUTE_API
        );
        let path = format!(
            "/subscriptions/{}/providers/Microsoft.Compute/virtualMachines",
            self.settings.subscription_id
        );
        let mut next = Some(url);
        let mut visited = BTreeSet::new();
        let mut instances = Vec::new();
        while let Some(url) = next {
            if !visited.insert(url.clone()) || visited.len() > 1000 {
                return Err(failure(
                    "pagination",
                    "Azure VM inventory pagination did not make bounded progress",
                ));
            }
            let page: AzureVmPage =
                serde_json::from_value(self.get_management(&url, &path, deadline).await?)
                    .map_err(|_| failure("protocol", "Azure VM inventory response was invalid"))?;
            for vm in page.value {
                if vm.tags.get("garm-controller-id") != Some(&scope.controller_id) {
                    continue;
                }
                if vm.name.is_empty()
                    || !valid_vm_resource_id(&vm.id, &self.settings.subscription_id)
                    || !self
                        .settings
                        .locations
                        .values()
                        .any(|location| location.location == vm.location)
                    || !vm
                        .id
                        .rsplit('/')
                        .next()
                        .is_some_and(|name| name.eq_ignore_ascii_case(&vm.name))
                {
                    return Err(failure(
                        "protocol",
                        "Azure owned VM has an invalid native identity",
                    ));
                }
                if scope
                    .resource_name
                    .as_ref()
                    .is_some_and(|name| name != &vm.name)
                {
                    continue;
                }
                instances.push(provider_instance(vm, &self.settings));
            }
            next = page.next_link;
        }
        Ok(instances)
    }
}
