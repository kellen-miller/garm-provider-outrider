use super::connection::{parse_json_under_deadline, path_segment, resource_url};
use super::{
    BTreeSet, CapacityAdviceRequest, CapacityAdviceResponse, CapacityCandidate, CapacityDisk,
    CapacityDistributionPolicy, CapacityFlexibilityPolicy, CapacityInstanceProperties,
    CapacityInstanceSelections, CapacityScheduling, CapacityZone, CatalogOffer, CatalogQuery,
    CloudError, Decimal, GcpBackend, GcpCatalogMachineFacts, GcpDiskType, GcpImage,
    GcpImageResource, GcpLaunchFacts, GcpMachineType, Instant, MAX_API_RESPONSE_BYTES,
    ProviderLaunchFacts, Url, failure, image_path_parts, timeout_at,
};

impl GcpBackend {
    pub(super) async fn image(
        &self,
        binding: &GcpImage,
        deadline: Instant,
    ) -> Result<(GcpImageResource, Option<String>), CloudError> {
        let (project, name) = image_path_parts(&self.settings.project_id, &binding.image_id)?;
        let path = format!(
            "/compute/v1/projects/{}/global/images/{}",
            path_segment(project)?,
            path_segment(name)?
        );
        let (image, request_id) = self
            .request_json(&self.settings.compute_endpoint, &path, deadline)
            .await?;
        Ok((image, request_id))
    }

    pub(super) async fn capacity_hint(
        &self,
        machine: &GcpMachineType,
        purchasing: &str,
        deadline: Instant,
    ) -> Option<crate::policy::NativeCapacityHint> {
        if !self.settings.capacity_advice_enabled || purchasing != "spot" {
            return None;
        }
        let region = self
            .settings
            .zone
            .rsplit_once('-')
            .map(|(region, _)| region)?;
        let request = CapacityAdviceRequest {
            instance_properties: CapacityInstanceProperties {
                scheduling: CapacityScheduling {
                    provisioning_model: "SPOT".to_owned(),
                },
            },
            instance_flexibility_policy: CapacityFlexibilityPolicy {
                instance_selections: CapacityInstanceSelections {
                    candidate: CapacityCandidate {
                        machine_types: vec![machine.name.clone()],
                        guest_accelerators: Vec::new(),
                        disks: if machine.bundled_local_ssds.is_none() {
                            Vec::new()
                        } else {
                            vec![CapacityDisk {
                                r#type: "SCRATCH".to_owned(),
                            }]
                        },
                    },
                },
            },
            size: 1,
            distribution_policy: CapacityDistributionPolicy {
                zones: vec![CapacityZone {
                    zone: format!("zones/{}", self.settings.zone),
                }],
                target_shape: "ANY_SINGLE_ZONE".to_owned(),
            },
        };
        let path = format!(
            "/compute/beta/projects/{}/regions/{}/advice/capacity",
            path_segment(&self.settings.project_id).ok()?,
            path_segment(region).ok()?
        );
        let url = resource_url(&self.settings.compute_endpoint, &path).ok()?;
        let token = timeout_at(deadline, self.credentials.access_token())
            .await
            .ok()?
            .ok()?;
        let response = timeout_at(
            deadline,
            self.client
                .post(url)
                .bearer_auth(&token.token)
                .json(&request)
                .send(),
        )
        .await
        .ok()?
        .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let bytes = timeout_at(deadline, response.bytes()).await.ok()?.ok()?;
        if bytes.len() > MAX_API_RESPONSE_BYTES {
            return None;
        }
        let response: CapacityAdviceResponse = parse_json_under_deadline(bytes.to_vec(), deadline)
            .await
            .ok()?;
        if response.recommendations.len() != 1 {
            return None;
        }
        let mut scores = Vec::new();
        for recommendation in response.recommendations {
            if recommendation.shards.len() != 1 {
                return None;
            }
            let shard = &recommendation.shards[0];
            let zone = shard.zone.rsplit('/').next().unwrap_or(&shard.zone);
            if zone != self.settings.zone
                || shard.machine_type != machine.name
                || shard.provisioning_model != "SPOT"
                || shard.instance_count != 1
            {
                return None;
            }
            let score = recommendation
                .scores
                .and_then(|scores| scores.obtainability)?;
            if !(Decimal::ZERO..=Decimal::ONE).contains(&score) {
                return None;
            }
            scores.push(score);
        }
        if scores.len() != 1 {
            return None;
        }
        Some(crate::policy::NativeCapacityHint {
            source: format!(
                "compute.beta.advice.capacity.obtainability;region={region};zone={};size=1;selection=candidate",
                self.settings.zone
            ),
            cloud: "gcp".to_owned(),
            account: self.settings.project_id.clone(),
            location: self.settings.pricing_location.clone(),
            observed_at_unix_seconds: unix_now(),
            score: scores[0],
        })
    }

    pub(super) fn query_matches_scope(&self, query: &CatalogQuery) -> bool {
        query.clouds.iter().any(|cloud| cloud == "gcp")
            && (query.accounts.is_empty() || query.accounts.contains(&self.settings.project_id))
            && (query.locations.is_empty()
                || query.locations.contains(&self.settings.pricing_location))
            && (query.profile.accounts.is_empty()
                || query.profile.accounts.contains(&self.settings.project_id))
            && (query.profile.locations.is_empty()
                || query
                    .profile
                    .locations
                    .contains(&self.settings.pricing_location))
    }

    pub(super) fn validate_offer<'a>(
        &self,
        offer: &'a CatalogOffer,
    ) -> Result<&'a GcpLaunchFacts, CloudError> {
        let ProviderLaunchFacts::Gcp(facts) = &offer.launch_facts else {
            return Err(failure("configuration", "offer is not a GCP launch"));
        };
        let expected_disk_type = self
            .settings
            .storage_classes
            .get(&offer.candidate.boot_disk_class)
            .ok_or_else(|| failure("configuration", "GCP boot disk class is no longer bound"))?;
        if facts.project_id != self.settings.project_id
            || facts.zone != self.settings.zone
            || offer.selection.binding_id != self.settings.binding_id
            || facts.network_id != self.child.network_id
            || facts.subnetwork_id != self.child.subnetwork_id
            || facts.external_ip_access != self.child.external_ip_access
            || offer.candidate.cloud != "gcp"
            || offer.candidate.account != self.settings.project_id
            || offer.candidate.location != self.settings.pricing_location
            || !self.provider_supports(&offer.candidate.purchasing)
            || offer.selection.source_revision != self.settings.source_revision
            || offer.selection.binary_sha256 != self.settings.binary_sha256
            || offer.selection.configuration_sha256 != self.settings.configuration_sha256
            || offer.selection.flavor != offer.candidate.machine_type
            || offer.selection.approved_extras.len() != 3
            || offer
                .selection
                .approved_extras
                .get("spot")
                .and_then(serde_json::Value::as_bool)
                != Some(offer.candidate.purchasing == "spot")
            || offer
                .selection
                .approved_extras
                .get("disksize")
                .and_then(serde_json::Value::as_u64)
                != Some(u64::from(offer.candidate.boot_disk_gib))
            || offer
                .selection
                .approved_extras
                .get("disktype")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|value| {
                    value != expected_disk_type
                        && !value.ends_with(&format!("/diskTypes/{expected_disk_type}"))
                })
        {
            return Err(failure(
                "configuration",
                "GCP offer project, location, machine, or child launch facts changed",
            ));
        }
        Ok(facts)
    }

    pub(super) fn provider_supports(&self, purchasing: &str) -> bool {
        self.settings.purchasing_capabilities.contains(purchasing)
    }

    pub(super) fn machine_architecture(
        &self,
        machine: &GcpMachineType,
    ) -> Result<Option<String>, CloudError> {
        let native = machine
            .architecture
            .as_deref()
            .map(canonical_architecture)
            .transpose()?;
        let attested = self
            .settings
            .machine_facts
            .get(&machine.name)
            .and_then(|facts| facts.architecture.as_deref())
            .map(canonical_architecture)
            .transpose()?;
        if native
            .as_ref()
            .zip(attested.as_ref())
            .is_some_and(|(native, attested)| native != attested)
        {
            return Err(failure(
                "configuration",
                "GCP native and attested machine architectures conflict",
            ));
        }

        Ok(native.or(attested))
    }

    pub(super) async fn catalog_machine_facts(
        &self,
        machine: &GcpMachineType,
        profile: &crate::config::RunnerProfile,
        deadline: Instant,
        exclusion_counts: &mut super::super::CatalogExclusions,
    ) -> Result<Option<GcpCatalogMachineFacts>, CloudError> {
        let image = self
            .settings
            .images
            .get(&profile.image)
            .ok_or_else(|| failure("configuration", "GCP image alias is not bound"))?
            .clone();
        let memory_gib = Decimal::from(machine.memory_mb) / Decimal::from(1024u32);
        let family = machine.name.split('-').next().unwrap_or_default();
        if image.architecture != profile.architecture
            || image.os != profile.os
            || machine.guest_cpus < profile.vcpu_min
            || machine.guest_cpus > profile.vcpu_max
            || memory_gib < Decimal::from(profile.memory_gib_min)
            || memory_gib > Decimal::from(profile.memory_gib_max)
            || !profile
                .machine_types
                .get("gcp")
                .is_none_or(|types| types.is_empty() || types.contains(&machine.name))
            || profile.machine_families.get("gcp").is_some_and(|families| {
                !families.is_empty() && !families.iter().any(|allowed| allowed == family)
            })
            || profile
                .administrator_limits
                .allowed_machine_types
                .get("gcp")
                .is_some_and(|types| !types.is_empty() && !types.contains(&machine.name))
            || profile
                .administrator_limits
                .allowed_machine_families
                .get("gcp")
                .is_some_and(|families| !families.is_empty() && !families.contains(family))
            || !profile.capabilities.is_subset(&image.capabilities)
            || profile.gpu.is_some() && machine.accelerators.is_empty()
            || machine.accelerators.len() > 1
        {
            exclusion_counts.record("machine or image does not meet requested platform, compute, family, capability, or GPU bounds");
            return Ok(None);
        }

        let Some(machine_architecture) = self.machine_architecture(machine)? else {
            exclusion_counts.record("machine type lacks native or attested architecture facts");
            return Ok(None);
        };
        if machine_architecture != profile.architecture {
            exclusion_counts.record("machine architecture does not meet requested platform");
            return Ok(None);
        }

        let (image_resource, _) = self.image(&image, deadline).await?;
        if image_resource.status != "READY"
            || image_resource_id(&image_resource.self_link).as_deref()
                != Some(image.image_id.as_str())
            || image_resource.licenses.iter().collect::<BTreeSet<_>>()
                != image.license_facts.keys().collect::<BTreeSet<_>>()
        {
            exclusion_counts
                .record("image readiness, identity, or license declarations do not match");
            return Ok(None);
        }

        let Some(image_architecture) = image_resource.architecture.as_deref() else {
            exclusion_counts.record("image lacks native architecture facts");
            return Ok(None);
        };
        if canonical_architecture(image_architecture)? != profile.architecture {
            exclusion_counts.record("image architecture does not meet requested platform");
            return Ok(None);
        }

        let minimum_disk_gib = image_resource
            .disk_size_gb
            .as_deref()
            .and_then(|size| size.parse::<u32>().ok())
            .ok_or_else(|| failure("response", "GCP image has no valid diskSizeGb"))?;
        Ok(Some(GcpCatalogMachineFacts {
            image,
            image_resource,
            minimum_disk_gib,
            boot_disk_gib: profile.boot_disk.size_gib.max(minimum_disk_gib),
            memory_gib,
        }))
    }

    pub(super) async fn catalog_machine_inventory(
        &self,
        deadline: Instant,
    ) -> Result<Vec<GcpMachineType>, CloudError> {
        let path = format!(
            "/compute/v1/projects/{}/zones/{}/machineTypes",
            path_segment(&self.settings.project_id)?,
            path_segment(&self.settings.zone)?
        );
        let (machines, _) = self
            .list_all::<GcpMachineType>(&self.settings.compute_endpoint, &path, deadline)
            .await?;
        Ok(machines)
    }

    pub(super) async fn catalog_disk_inventory(
        &self,
        deadline: Instant,
    ) -> Result<Vec<GcpDiskType>, CloudError> {
        let path = format!(
            "/compute/v1/projects/{}/zones/{}/diskTypes",
            path_segment(&self.settings.project_id)?,
            path_segment(&self.settings.zone)?
        );
        let (disk_types, _) = self
            .list_all::<GcpDiskType>(&self.settings.compute_endpoint, &path, deadline)
            .await?;
        Ok(disk_types)
    }

    pub(super) fn gpu_matches_profile(
        &self,
        machine: &GcpMachineType,
        profile: &crate::config::RunnerProfile,
    ) -> bool {
        let Some(accelerator) = machine.accelerators.first() else {
            return profile.gpu.is_none();
        };
        let Some(facts) = self
            .settings
            .accelerator_facts
            .get(&accelerator.guest_accelerator_type)
        else {
            return false;
        };
        profile.gpu.as_ref().is_none_or(|requirement| {
            requirement.count <= accelerator.guest_accelerator_count
                && requirement.vendor == facts.vendor
                && requirement
                    .model
                    .as_ref()
                    .is_none_or(|model| model == &facts.model)
                && requirement.memory_gib.is_none_or(|memory| {
                    Decimal::from(memory)
                        <= facts.memory_gib * Decimal::from(accelerator.guest_accelerator_count)
                })
        }) && profile
            .administrator_limits
            .allowed_gpu_vendors
            .contains(&facts.vendor)
            && profile
                .administrator_limits
                .allowed_gpu_models
                .contains(&facts.model)
    }
}

pub(super) fn machine_type_usable(machine: &GcpMachineType) -> bool {
    machine
        .deprecated
        .as_ref()
        .is_none_or(|deprecated| deprecated.state == "ACTIVE")
}

pub(super) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub(super) fn canonical_architecture(value: &str) -> Result<String, CloudError> {
    match value.to_ascii_lowercase().as_str() {
        "x86_64" | "x86-64" | "amd64" => Ok("amd64".to_owned()),
        "arm64" | "aarch64" => Ok("arm64".to_owned()),
        _ => Err(failure(
            "unsupported",
            "GCP machine or image architecture is unknown",
        )),
    }
}

pub(super) fn market_choices(_purchasing: &str) -> Vec<&'static str> {
    vec!["spot", "on_demand"]
}

pub(super) fn image_resource_id(self_link: &str) -> Option<String> {
    let url = Url::parse(self_link).ok()?;
    if url.scheme() != "https" || url.host_str()? != "www.googleapis.com" {
        return None;
    }
    let resource = url.path().strip_prefix("/compute/v1/")?;
    let (project, name) = resource
        .strip_prefix("projects/")?
        .split_once("/global/images/")?;
    (!project.is_empty() && !name.is_empty() && !name.contains('/')).then(|| resource.to_owned())
}

pub(super) fn disk_size_is_valid(valid_disk_size: Option<&str>, size_gib: u32) -> bool {
    let Some(range) = valid_disk_size else {
        return false;
    };
    let Some((minimum, maximum)) = range.split_once('-') else {
        return false;
    };
    let parse = |value: &str| {
        value
            .trim_matches(|character: char| !character.is_ascii_digit())
            .parse::<u32>()
            .ok()
    };
    let (Some(minimum), Some(maximum)) = (parse(minimum), parse(maximum)) else {
        return false;
    };
    size_gib >= minimum && size_gib <= maximum
}
