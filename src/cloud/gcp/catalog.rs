use super::billing::{component, included_component};
use super::discovery::unix_now;

use super::connection::path_segment;
use super::discovery::{
    canonical_architecture, disk_size_is_valid, image_resource_id, machine_type_usable,
    market_choices,
};
use super::identity::map_instance;
use super::{
    BTreeMap, BTreeSet, Candidate, CatalogOffer, CatalogQuery, CloudCatalog, CloudError, Decimal,
    Duration, GcpBackend, GcpDiskType, GcpInstance, GcpLaunchFacts, GcpMachineType, Instant,
    OwnershipScope, PlacementIdentity, PriceQuote, ProviderInstance, ProviderLaunchFacts,
    ProviderSelection, failure,
};

#[async_trait::async_trait]
impl CloudCatalog for GcpBackend {
    async fn catalog(
        &self,
        query: CatalogQuery,
        deadline: Instant,
    ) -> Result<crate::cloud::CatalogDiscovery, CloudError> {
        if !self.query_matches_scope(&query) {
            return Ok(crate::cloud::CatalogDiscovery::default());
        }
        let machines = self.catalog_machine_inventory(deadline).await?;
        let mut offers = Vec::new();
        let mut exclusion_counts = super::super::CatalogExclusions::default();
        for machine in machines {
            if !machine_type_usable(&machine) {
                exclusion_counts.record("machine type is deprecated");
                continue;
            }
            let Some(machine_facts_for_catalog) = self
                .catalog_machine_facts(&machine, &query.profile, deadline, &mut exclusion_counts)
                .await?
            else {
                exclusion_counts.record("boot disk class lacks exact performance facts");
                continue;
            };
            let image = machine_facts_for_catalog.image;
            let image_resource = machine_facts_for_catalog.image_resource;
            let minimum_disk = machine_facts_for_catalog.minimum_disk_gib;
            let boot_disk = machine_facts_for_catalog.boot_disk_gib;
            let memory_gib = machine_facts_for_catalog.memory_gib;
            let Some(disk_performance) = self
                .settings
                .disk_performance
                .get(&query.profile.boot_disk.class)
            else {
                continue;
            };
            let vcpu = machine.guest_cpus;
            let disk_types = self.catalog_disk_inventory(deadline).await?;
            let requested_disk_class = query.profile.boot_disk.class.as_str();
            let disk_type = self
                .settings
                .storage_classes
                .get(requested_disk_class)
                .ok_or_else(|| failure("unsupported", "GCP boot disk class is not bound"))?;
            let Some(disk_resource) = disk_types.iter().find(|disk| {
                (disk.name == *disk_type || disk.self_link == *disk_type)
                    && disk.deprecated.is_none()
            }) else {
                exclusion_counts.record("boot disk type is unavailable in the configured zone");
                continue;
            };
            if !disk_size_is_valid(disk_resource.valid_disk_size.as_deref(), boot_disk) {
                exclusion_counts.record("boot disk size is outside the provider limits");
                continue;
            }
            if query.profile.local_scratch_gib > 0 && machine.bundled_local_ssds.is_none() {
                exclusion_counts.record("machine type does not provide bundled local SSDs");
                continue;
            }
            let bundled_local_ssd_partitions = machine
                .bundled_local_ssds
                .iter()
                .map(|ssd| ssd.partition_count)
                .sum::<u32>();
            let machine_facts = self.settings.machine_facts.get(&machine.name);
            let Some(machine_facts) = machine_facts else {
                exclusion_counts.record("machine type lacks exact billing and disk facts");
                continue;
            };
            let Some(machine_disk_iops_cap) = machine_facts.max_pd_iops else {
                exclusion_counts.record("machine type lacks maximum boot disk IOPS");
                continue;
            };
            let Some(machine_disk_throughput_cap) = machine_facts.max_pd_throughput_mib_s else {
                exclusion_counts.record("machine type lacks maximum boot disk throughput");
                continue;
            };
            let actual_disk_iops = disk_performance
                .base_iops
                .saturating_add(boot_disk.saturating_mul(disk_performance.iops_per_gib))
                .min(disk_performance.maximum_iops)
                .min(machine_disk_iops_cap);
            let actual_disk_throughput = (disk_performance.base_throughput_mib_s
                + Decimal::from(boot_disk) * disk_performance.throughput_per_gib_mib_s)
                .min(disk_performance.maximum_throughput_mib_s)
                .min(machine_disk_throughput_cap);
            if query
                .profile
                .boot_disk
                .iops
                .is_some_and(|required| required > actual_disk_iops)
                || query
                    .profile
                    .boot_disk
                    .throughput_mib_s
                    .is_some_and(|required| Decimal::from(required) > actual_disk_throughput)
            {
                exclusion_counts.record("boot disk cannot meet requested IOPS or throughput");
                continue;
            }
            if bundled_local_ssd_partitions > 0
                && (machine_facts.bundled_local_ssd_partitions
                    != Some(bundled_local_ssd_partitions)
                    || machine_facts.bundled_local_ssd_gib.is_none()
                    || machine_facts.local_ssd_billing_sku.is_none())
            {
                exclusion_counts.record("bundled local SSD facts do not match the machine type");
                continue;
            }
            let bundled_local_ssd_gib = machine_facts.bundled_local_ssd_gib.unwrap_or(0);
            if Decimal::from(bundled_local_ssd_gib) < Decimal::from(query.profile.local_scratch_gib)
            {
                exclusion_counts.record("bundled local SSD capacity is below the request");
                continue;
            }
            for purchasing in market_choices(&query.profile.purchasing) {
                if !self.provider_supports(purchasing)
                    || !query.profile.purchasing_models.contains(purchasing)
                    || !query
                        .profile
                        .administrator_limits
                        .allowed_purchasing
                        .contains(purchasing)
                {
                    exclusion_counts.record("purchasing model is not supported or permitted");
                    continue;
                }
                if !machine.accelerators.is_empty() && purchasing != "spot" {
                    exclusion_counts
                        .record("attached accelerator machine requires spot purchasing");
                    continue;
                }
                if machine.is_shared_cpu && machine_facts.shared_cpu_billing_sku.is_none() {
                    exclusion_counts.record("shared CPU machine lacks an exact billing SKU");
                    continue;
                }
                if !self.gpu_matches_profile(&machine, &query.profile) {
                    exclusion_counts
                        .record("machine accelerator facts do not meet the GPU request");
                    continue;
                }
                let profile_gpu = if machine.accelerators.is_empty() {
                    None
                } else {
                    let accelerator = &machine.accelerators[0];
                    let facts = self
                        .settings
                        .accelerator_facts
                        .get(&accelerator.guest_accelerator_type);
                    if let Some(facts) = facts {
                        Some((accelerator, facts))
                    } else {
                        exclusion_counts.record("accelerator lacks exact central facts");
                        continue;
                    }
                };
                let identity = format!(
                    "gcp/{}/{}/{}/{}/{}/{}",
                    self.settings.project_id,
                    self.settings.zone,
                    machine.name,
                    image.image_id,
                    disk_type,
                    purchasing
                );
                let image_id = image_resource_id(&image_resource.self_link)
                    .ok_or_else(|| failure("response", "GCP image selfLink is invalid"))?;
                let selected_disk = boot_disk;
                let mut candidate = Candidate {
                    identity,
                    os: image.os.clone(),
                    architecture: query.profile.architecture.clone(),
                    image: query.profile.image.clone(),
                    vcpu,
                    memory_gib,
                    machine_type: machine.name.clone(),
                    machine_family: machine.name.split('-').next().map(str::to_owned),
                    cloud: "gcp".to_owned(),
                    purchasing: purchasing.to_owned(),
                    account: self.settings.project_id.clone(),
                    location: self.settings.pricing_location.clone(),
                    gpu_vendor: profile_gpu.map(|(_, facts)| facts.vendor.clone()),
                    gpu_model: profile_gpu.map(|(_, facts)| facts.model.clone()),
                    gpu_count: profile_gpu
                        .map_or(0, |(accelerator, _)| accelerator.guest_accelerator_count),
                    gpu_memory_gib: profile_gpu.map(|(accelerator, facts)| {
                        facts.memory_gib * Decimal::from(accelerator.guest_accelerator_count)
                    }),
                    boot_disk_class: requested_disk_class.to_owned(),
                    boot_disk_gib: selected_disk,
                    boot_disk_iops: actual_disk_iops,
                    boot_disk_throughput_mib_s: actual_disk_throughput,
                    local_scratch_gib: Decimal::from(bundled_local_ssd_gib),
                    capabilities: image.capabilities.iter().cloned().collect(),
                    price: None,
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
                candidate.native_capacity_hint =
                    self.capacity_hint(&machine, purchasing, deadline).await;
                offers.push(CatalogOffer {
                    candidate,
                    selection: ProviderSelection {
                        binding_id: self.settings.binding_id.clone(),
                        source_revision: self.settings.source_revision.clone(),
                        binary_sha256: self.settings.binary_sha256.clone(),
                        configuration_sha256: self.settings.configuration_sha256.clone(),
                        flavor: machine.name.clone(),
                        image: image.image_id.clone(),
                        approved_extras: BTreeMap::from([
                            ("disksize".to_owned(), serde_json::json!(selected_disk)),
                            (
                                "disktype".to_owned(),
                                serde_json::json!(disk_resource.self_link),
                            ),
                            ("spot".to_owned(), serde_json::json!(purchasing == "spot")),
                        ]),
                    },
                    launch_facts: ProviderLaunchFacts::Gcp(GcpLaunchFacts {
                        project_id: self.settings.project_id.clone(),
                        zone: self.settings.zone.clone(),
                        network_id: self.child.network_id.clone(),
                        subnetwork_id: self.child.subnetwork_id.clone(),
                        external_ip_access: self.child.external_ip_access,
                        image_id,
                        image_minimum_disk_gib: minimum_disk,
                        machine_type_accelerators: machine
                            .accelerators
                            .iter()
                            .map(|accelerator| {
                                (
                                    accelerator.guest_accelerator_type.clone(),
                                    accelerator.guest_accelerator_count,
                                )
                            })
                            .collect(),
                        bundled_local_ssd_partitions,
                    }),
                });
            }
        }
        Ok(crate::cloud::CatalogDiscovery {
            offers,
            exclusions: exclusion_counts.into_catalog_exclusions(),
            queried: true,
        })
    }

    async fn quote(
        &self,
        offer: &CatalogOffer,
        duration: Duration,
        _maximum_price_age: Duration,
        deadline: Instant,
    ) -> Result<PriceQuote, CloudError> {
        let facts = self.validate_offer(offer)?;
        let expected_disk_type = self
            .settings
            .storage_classes
            .get(&offer.candidate.boot_disk_class)
            .ok_or_else(|| failure("configuration", "GCP boot disk class is no longer bound"))?;
        let machine_path = format!(
            "/compute/v1/projects/{}/zones/{}/machineTypes/{}",
            path_segment(&self.settings.project_id)?,
            path_segment(&self.settings.zone)?,
            path_segment(&offer.selection.flavor)?
        );
        let (machine, _) = self
            .request_json::<GcpMachineType>(
                &self.settings.compute_endpoint,
                &machine_path,
                deadline,
            )
            .await?;
        if machine.name != offer.selection.flavor
            || !machine_type_usable(&machine)
            || machine.guest_cpus != offer.candidate.vcpu
            || Decimal::from(machine.memory_mb) / Decimal::from(1024u32)
                != offer.candidate.memory_gib
            || machine
                .accelerators
                .iter()
                .map(|accel| {
                    (
                        accel.guest_accelerator_type.clone(),
                        accel.guest_accelerator_count,
                    )
                })
                .collect::<Vec<_>>()
                != facts.machine_type_accelerators
            || !machine.accelerators.is_empty() && offer.candidate.purchasing != "spot"
            || machine
                .bundled_local_ssds
                .iter()
                .map(|ssd| ssd.partition_count)
                .sum::<u32>()
                != facts.bundled_local_ssd_partitions
            || machine
                .architecture
                .as_deref()
                .and_then(|value| canonical_architecture(value).ok())
                .as_deref()
                != Some(offer.candidate.architecture.as_str())
        {
            return Err(failure(
                "configuration",
                "GCP selected machine hardware changed after catalog selection",
            ));
        }
        let machine_facts = self
            .settings
            .machine_facts
            .get(&machine.name)
            .ok_or_else(|| {
                failure(
                    "configuration",
                    "GCP selected machine facts are no longer bound",
                )
            })?;
        if machine_facts.bundled_local_ssd_partitions.unwrap_or(0)
            != facts.bundled_local_ssd_partitions
            || Decimal::from(machine_facts.bundled_local_ssd_gib.unwrap_or(0))
                != offer.candidate.local_scratch_gib
        {
            return Err(failure(
                "configuration",
                "GCP bundled Local SSD capacity changed after catalog selection",
            ));
        }
        let image = self
            .settings
            .images
            .get(&offer.candidate.image)
            .ok_or_else(|| failure("configuration", "GCP image alias is not bound"))?;
        if offer.selection.image != image.image_id
            || offer.selection.image != facts.image_id
            || offer.candidate.os != image.os
        {
            return Err(failure(
                "configuration",
                "GCP selected image alias or OS facts changed after catalog selection",
            ));
        }
        let selected_disk_type = offer
            .selection
            .approved_extras
            .get("disktype")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| failure("configuration", "GCP selected disk type is missing"))?;
        let disk_type_path = format!(
            "/compute/v1/projects/{}/zones/{}/diskTypes",
            path_segment(&self.settings.project_id)?,
            path_segment(&self.settings.zone)?
        );
        let (disk_types, _) = self
            .list_all::<GcpDiskType>(&self.settings.compute_endpoint, &disk_type_path, deadline)
            .await?;
        let Some(disk_type) = disk_types.iter().find(|disk| {
            (disk.self_link == selected_disk_type || disk.name == selected_disk_type)
                && disk.deprecated.is_none()
                && (disk.name == *expected_disk_type || disk.self_link == *expected_disk_type)
        }) else {
            return Err(failure(
                "configuration",
                "GCP selected boot disk type changed after catalog selection",
            ));
        };
        if !disk_size_is_valid(
            disk_type.valid_disk_size.as_deref(),
            offer.candidate.boot_disk_gib,
        ) || offer.candidate.boot_disk_gib < facts.image_minimum_disk_gib
        {
            return Err(failure(
                "configuration",
                "GCP selected boot disk size is outside the exact disk type range",
            ));
        }
        let (image_resource, _) = self.image(image, deadline).await?;
        if image_resource.status != "READY"
            || image_resource_id(&image_resource.self_link).as_deref()
                != Some(image.image_id.as_str())
            || facts.image_id != image.image_id
            || image_resource
                .disk_size_gb
                .as_deref()
                .and_then(|size| size.parse::<u32>().ok())
                != Some(facts.image_minimum_disk_gib)
            || image_resource
                .architecture
                .as_deref()
                .and_then(|value| canonical_architecture(value).ok())
                .as_deref()
                != Some(offer.candidate.architecture.as_str())
        {
            return Err(failure(
                "configuration",
                "GCP selected image changed after catalog selection",
            ));
        }
        if image_resource.licenses.iter().collect::<BTreeSet<_>>()
            != image.license_facts.keys().collect::<BTreeSet<_>>()
        {
            return Err(failure(
                "configuration",
                "GCP immutable image license metadata changed or is unattested",
            ));
        }
        let price_observed_at = unix_now();
        let prices = self
            .price_set(
                &machine,
                offer.candidate.boot_disk_class.as_str(),
                offer.candidate.boot_disk_gib,
                image,
                &offer.candidate.purchasing,
                deadline,
            )
            .await
            .map_err(CloudError::price_unavailable)?;
        let compute_component = component(
            prices.compute,
            &format!("Cloud Billing SKUs {}", prices.compute_source),
            60,
            Some(prices.compute_effective_at),
        );
        let disk_component = component(
            prices.disk.checked_add(prices.local_ssd).ok_or_else(|| {
                CloudError::price_unavailable(failure("pricing", "GCP disk charge overflow"))
            })?,
            &format!("Cloud Billing SKUs {}", prices.disk_source),
            0,
            Some(prices.disk_effective_at),
        );
        let licensing_component = if prices.license == Decimal::ZERO {
            included_component(&prices.license_source)
        } else {
            component(
                prices.license,
                &prices.license_source,
                60,
                prices.license_effective_at,
            )
        };
        let public_address = if let Some(source) = prices.public_ip_source.as_deref() {
            component(
                prices.public_ip,
                &format!("Cloud Billing SKU {source}"),
                0,
                prices.public_ip_effective_at,
            )
        } else {
            included_component("GCP child external IP access is disabled")
        };
        let mut quote = PriceQuote {
            currency: "USD".to_owned(),
            provider_request_ids: prices.request_ids,
            compute: compute_component,
            disk: disk_component,
            licensing: licensing_component,
            public_address,
            new_host_minimum_commitment_seconds: None,
            existing_host_remaining_commitment_seconds: None,
            observed_age_seconds: Some(0),
            included_charge_names: BTreeSet::new(),
            exclusions: vec!["GCP network egress and taxes are excluded".to_owned()],
        };
        quote.mark_observed_at(price_observed_at);
        quote.estimated_total(duration.as_secs()).map_err(|_| {
            CloudError::price_unavailable(failure("pricing", "GCP quote could not be normalized"))
        })?;
        Ok(quote)
    }

    async fn inspect(
        &self,
        identity: &PlacementIdentity,
        deadline: Instant,
    ) -> Result<Option<ProviderInstance>, CloudError> {
        if identity.controller_id.is_empty()
            || identity.cloud != "gcp"
            || identity.account_id != self.settings.project_id
            || identity.location != self.settings.zone
            || identity.resource_id.is_empty()
            || !identity
                .resource_id
                .bytes()
                .all(|byte| byte.is_ascii_digit())
            || identity
                .dependency_ids
                .get("instance_name")
                .is_none_or(String::is_empty)
        {
            return Err(failure(
                "ownership",
                "GCP inspect identity is malformed or outside the configured scope",
            ));
        }
        let path = format!(
            "/compute/v1/projects/{}/zones/{}/instances",
            path_segment(&self.settings.project_id)?,
            path_segment(&self.settings.zone)?
        );
        let (instances, _) = self
            .list_all::<GcpInstance>(&self.settings.compute_endpoint, &path, deadline)
            .await?;
        for instance in instances {
            if instance.id == identity.resource_id {
                if identity.dependency_ids.get("instance_name") != Some(&instance.name) {
                    return Err(failure(
                        "ownership",
                        "GCP native instance name changed for the observed numeric ID",
                    ));
                }
                let mapped = map_instance(
                    &self.settings,
                    &self.child,
                    identity.controller_id.clone(),
                    instance,
                )?;
                if mapped.ownership_tags.is_empty() {
                    return Err(failure(
                        "ownership",
                        "known GCP instance ID belongs to another controller",
                    ));
                }
                return Ok(Some(mapped));
            }
        }
        Ok(None)
    }

    async fn list(
        &self,
        scope: &OwnershipScope,
        deadline: Instant,
    ) -> Result<Vec<ProviderInstance>, CloudError> {
        if scope.controller_id.is_empty()
            || scope.resource_name.as_ref().is_some_and(String::is_empty)
        {
            return Err(failure(
                "ownership",
                "GCP list scope has an empty controller or runner name",
            ));
        }
        let path = format!(
            "/compute/v1/projects/{}/zones/{}/instances",
            path_segment(&self.settings.project_id)?,
            path_segment(&self.settings.zone)?
        );
        let (instances, _) = self
            .list_all::<GcpInstance>(&self.settings.compute_endpoint, &path, deadline)
            .await?;
        let mut seen = BTreeSet::new();
        let mut result = Vec::new();
        for instance in instances {
            let mapped = map_instance(
                &self.settings,
                &self.child,
                scope.controller_id.clone(),
                instance,
            )?;
            if mapped.ownership_tags.is_empty() {
                continue;
            }
            if !scope
                .resource_name
                .as_ref()
                .is_none_or(|name| name == &mapped.name)
            {
                continue;
            }
            if seen.insert(mapped.identity.resource_id.clone()) {
                result.push(mapped);
            }
        }
        Ok(result)
    }
}
