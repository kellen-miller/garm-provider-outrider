use super::{
    AwsBackend, AwsCatalogInventory, AwsImage, AwsLaunchFacts, AwsLocation, AwsSubnetFacts,
    BTreeMap, BTreeSet, Candidate, CatalogOffer, CloudError, Decimal, InstanceType, Instant,
    ProviderLaunchFacts, ProviderSelection, aws_machine_shape, failure, image_metadata_matches,
    sdk_failure, standard_usage_operation, timeout_at,
};

impl AwsBackend {
    pub(super) fn configured_location(&self, alias: &str) -> Result<&AwsLocation, CloudError> {
        self.settings
            .locations
            .get(alias)
            .ok_or_else(|| failure("configuration", "AWS location is not configured", vec![]))
    }

    pub(super) fn configured_image(&self, alias: &str) -> Result<&AwsImage, CloudError> {
        self.settings
            .images
            .get(alias)
            .ok_or_else(|| failure("configuration", "AWS image alias is not configured", vec![]))
    }

    pub(super) async fn subnet_facts(
        &self,
        location: &AwsLocation,
        deadline: Instant,
    ) -> Result<AwsSubnetFacts, CloudError> {
        let output = timeout_at(
            deadline,
            self.ec2
                .describe_subnets()
                .subnet_ids(&location.subnet_id)
                .send(),
        )
        .await
        .map_err(|_| failure("timeout", "AWS subnet lookup timed out", vec![]))?
        .map_err(|error| sdk_failure(&error, "AWS subnet lookup failed", vec![]))?;
        let subnet = output
            .subnets()
            .iter()
            .find(|subnet| subnet.subnet_id() == Some(location.subnet_id.as_str()))
            .ok_or_else(|| {
                failure(
                    "configuration",
                    "AWS configured subnet is unavailable",
                    vec![],
                )
            })?;
        let availability_zone = subnet.availability_zone().ok_or_else(|| {
            failure(
                "configuration",
                "AWS subnet availability zone is unavailable",
                vec![],
            )
        })?;
        let map_public_ip_on_launch = subnet.map_public_ip_on_launch().ok_or_else(|| {
            failure(
                "configuration",
                "AWS subnet public-IP behavior is unavailable",
                vec![],
            )
        })?;
        if availability_zone != location.availability_zone {
            return Err(failure(
                "configuration",
                "AWS subnet is in a different availability zone",
                vec![],
            ));
        }

        Ok(AwsSubnetFacts {
            availability_zone: availability_zone.to_owned(),
            map_public_ip_on_launch,
        })
    }

    pub(super) async fn spot_placement_hints(
        &self,
        machine_types: &[String],
        deadline: Instant,
    ) -> BTreeMap<String, crate::policy::NativeCapacityHint> {
        if !self.settings.include_spot_placement_scores
            || machine_types.is_empty()
            || machine_types.len() > 100
        {
            return BTreeMap::new();
        }

        let Some(zones) = self.spot_zones(deadline).await else {
            return BTreeMap::new();
        };

        let Some(scores) = self.spot_scores(machine_types, deadline).await else {
            return BTreeMap::new();
        };

        let observed_at_unix_seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|elapsed| elapsed.as_secs());
        let Some(observed_at_unix_seconds) = observed_at_unix_seconds else {
            return BTreeMap::new();
        };

        let source = format!(
            "aws:GetSpotPlacementScores;scope=availability-zone;target=1-unit;types={}",
            machine_types.join(",")
        );
        self.settings
            .locations
            .iter()
            .filter_map(|(alias, location)| {
                let (zone_id, region, state) = zones.get(&location.availability_zone)?;
                if region != &self.settings.region || state != "available" {
                    return None;
                }

                Some((
                    alias.clone(),
                    crate::policy::NativeCapacityHint {
                        source: source.clone(),
                        cloud: "aws".to_owned(),
                        account: self.settings.account_id.clone(),
                        location: alias.clone(),
                        observed_at_unix_seconds,
                        score: scores.get(zone_id)?.to_owned(),
                    },
                ))
            })
            .collect()
    }

    pub(super) async fn spot_zones(
        &self,
        deadline: Instant,
    ) -> Option<BTreeMap<String, (String, String, String)>> {
        let zone_names = self
            .settings
            .locations
            .values()
            .map(|location| location.availability_zone.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let output = timeout_at(
            deadline,
            self.ec2
                .describe_availability_zones()
                .set_zone_names(Some(zone_names))
                .send(),
        )
        .await
        .ok()?
        .ok()?;
        Some(
            output
                .availability_zones()
                .iter()
                .filter_map(|zone| {
                    Some((
                        zone.zone_name()?.to_owned(),
                        (
                            zone.zone_id()?.to_owned(),
                            zone.region_name()?.to_owned(),
                            zone.state()?.as_str().to_owned(),
                        ),
                    ))
                })
                .collect(),
        )
    }

    pub(super) async fn spot_scores(
        &self,
        machine_types: &[String],
        deadline: Instant,
    ) -> Option<BTreeMap<String, Decimal>> {
        let mut token = None;
        let mut scores = BTreeMap::new();
        loop {
            let output = timeout_at(
                deadline,
                self.ec2
                    .get_spot_placement_scores()
                    .set_instance_types(Some(machine_types.to_vec()))
                    .target_capacity(1)
                    .target_capacity_unit_type(aws_sdk_ec2::types::TargetCapacityUnitType::Units)
                    .single_availability_zone(true)
                    .region_names(self.settings.region.clone())
                    .set_next_token(token.clone())
                    .send(),
            )
            .await
            .ok()?
            .ok()?;
            for placement in output.spot_placement_scores() {
                if placement.region() != Some(self.settings.region.as_str())
                    || placement
                        .score()
                        .is_none_or(|score| !(1..=10).contains(&score))
                {
                    continue;
                }

                if let (Some(zone_id), Some(score)) =
                    (placement.availability_zone_id(), placement.score())
                {
                    scores.insert(zone_id.to_owned(), Decimal::from(score));
                }
            }

            token = output.next_token().map(str::to_owned);
            if token.is_none() {
                return Some(scores);
            }
        }
    }

    pub(super) async fn fetch_catalog_image(
        &self,
        profile: &crate::config::RunnerProfile,
        deadline: Instant,
    ) -> Result<(&AwsImage, String, u32), CloudError> {
        if profile.os == "macos" {
            return Err(failure(
                "unsupported",
                "AWS macOS requires upstream GARM/common/AWS provider support",
                vec![],
            ));
        }

        let image = self.configured_image(&profile.image)?;
        if image.os != profile.os || image.architecture != profile.architecture {
            return Err(failure(
                "configuration",
                "configured AWS image does not match requested OS and architecture",
                vec![],
            ));
        }

        if (profile.os == "linux" && !image.cloud_init)
            || (profile.os == "windows" && !image.ec2_launch_agent)
        {
            return Err(failure(
                "configuration",
                "configured AWS image lacks its required first-boot agent",
                vec![],
            ));
        }

        if !image.capabilities.is_superset(&profile.capabilities) {
            return Err(failure(
                "configuration",
                "configured AWS image lacks required capabilities",
                vec![],
            ));
        }

        let actual = self
            .fetch_image_metadata(&image.image_id, "catalog", deadline)
            .await?;
        if !image_metadata_matches(&actual, &profile.os, &profile.architecture) {
            return Err(failure(
                "configuration",
                "AWS AMI metadata does not match its configured OS and architecture",
                vec![],
            ));
        }

        let root_device_name = actual
            .root_device_name()
            .ok_or_else(|| {
                failure(
                    "configuration",
                    "AWS AMI does not report a root device name",
                    vec![],
                )
            })?
            .to_owned();
        let root_volume_minimum_gib = actual
            .block_device_mappings()
            .iter()
            .find(|mapping| mapping.device_name() == Some(root_device_name.as_str()))
            .and_then(|mapping| mapping.ebs())
            .and_then(|ebs| ebs.volume_size())
            .and_then(|size| u32::try_from(size).ok())
            .filter(|size| *size > 0)
            .ok_or_else(|| {
                failure(
                    "configuration",
                    "AWS AMI root EBS volume has no valid minimum size",
                    vec![],
                )
            })?;
        if root_volume_minimum_gib > profile.boot_disk.size_gib {
            return Err(failure(
                "unsupported",
                "AWS AMI root volume exceeds the configured boot disk size",
                vec![],
            ));
        }

        Ok((image, root_device_name, root_volume_minimum_gib))
    }

    pub(super) async fn fetch_image_metadata(
        &self,
        image_id: &str,
        purpose: &str,
        deadline: Instant,
    ) -> Result<aws_sdk_ec2::types::Image, CloudError> {
        let images = timeout_at(
            deadline,
            self.ec2.describe_images().image_ids(image_id).send(),
        )
        .await
        .map_err(|_| {
            failure(
                "timeout",
                &format!("AWS {purpose} image lookup timed out"),
                vec![],
            )
        })?
        .map_err(|error| {
            sdk_failure(
                &error,
                &format!("AWS {purpose} image lookup failed"),
                vec![],
            )
        })?;
        images
            .images()
            .iter()
            .find(|actual| actual.image_id() == Some(image_id))
            .cloned()
            .ok_or_else(|| {
                failure(
                    "configuration",
                    &format!("AWS {purpose} AMI is unavailable"),
                    vec![],
                )
            })
    }

    pub(super) async fn fetch_catalog_inventory(
        &self,
        requested_types: Option<&Vec<String>>,
        deadline: Instant,
    ) -> Result<AwsCatalogInventory, CloudError> {
        let mut type_token = None;
        let mut machine_types = Vec::new();
        loop {
            let output = timeout_at(
                deadline,
                self.ec2
                    .describe_instance_types()
                    .set_instance_types(requested_types.map(|types| {
                        types
                            .iter()
                            .map(|kind| InstanceType::from(kind.as_str()))
                            .collect()
                    }))
                    .set_next_token(type_token.clone())
                    .set_max_results(requested_types.is_none().then_some(100))
                    .send(),
            )
            .await
            .map_err(|_| failure("timeout", "AWS instance inventory timed out", vec![]))?
            .map_err(|error| sdk_failure(&error, "AWS instance inventory failed", vec![]))?;
            machine_types.extend_from_slice(output.instance_types());
            type_token = output.next_token().map(str::to_owned);
            if type_token.is_none() {
                break;
            }
        }

        let mut offering_token = None;
        let mut offered = BTreeSet::new();
        loop {
            let output = timeout_at(
                deadline,
                self.ec2
                    .describe_instance_type_offerings()
                    .location_type(aws_sdk_ec2::types::LocationType::AvailabilityZone)
                    .set_next_token(offering_token.clone())
                    .max_results(1000)
                    .send(),
            )
            .await
            .map_err(|_| failure("timeout", "AWS offering inventory timed out", vec![]))?
            .map_err(|error| sdk_failure(&error, "AWS offering inventory failed", vec![]))?;
            offered.extend(
                output
                    .instance_type_offerings()
                    .iter()
                    .filter_map(|offering| {
                        Some((
                            offering.instance_type()?.as_str().to_owned(),
                            offering.location()?.to_owned(),
                        ))
                    }),
            );
            offering_token = output.next_token().map(str::to_owned);
            if offering_token.is_none() {
                break;
            }
        }

        Ok(AwsCatalogInventory {
            machine_types,
            offered,
            locations: Vec::new(),
            spot_hints: BTreeMap::new(),
        })
    }

    pub(super) async fn catalog_spot_hints(
        &self,
        machine_types: &[aws_sdk_ec2::types::InstanceTypeInfo],
        requested_types: Option<&Vec<String>>,
        profile: &crate::config::RunnerProfile,
        deadline: Instant,
    ) -> BTreeMap<String, crate::policy::NativeCapacityHint> {
        if !profile.purchasing_models.contains("spot") {
            return BTreeMap::new();
        }

        let available: BTreeSet<String> = machine_types
            .iter()
            .filter_map(|info| info.instance_type().map(|kind| kind.as_str().to_owned()))
            .collect();
        let types: Vec<String> = match requested_types {
            Some(requested) => requested
                .iter()
                .filter(|kind| available.contains(*kind))
                .cloned()
                .collect(),
            None => available.iter().cloned().collect(),
        };

        self.spot_placement_hints(&types, deadline).await
    }

    pub(super) fn catalog_candidates(
        &self,
        profile: &crate::config::RunnerProfile,
        image: &AwsImage,
        inventory: &AwsCatalogInventory,
        root_device_name: &str,
        root_volume_minimum_gib: u32,
        exclusion_counts: &mut BTreeMap<String, usize>,
    ) -> Result<Vec<CatalogOffer>, CloudError> {
        if profile.boot_disk.size_gib < root_volume_minimum_gib {
            return Err(failure(
                "unsupported",
                "AWS AMI root volume exceeds the configured boot disk size",
                vec![],
            ));
        }

        let mut shapes = Vec::new();
        for info in &inventory.machine_types {
            if let Some(shape) = aws_machine_shape(info, profile, exclusion_counts)? {
                shapes.push(shape);
            }
        }

        let mut candidates = Vec::new();
        for shape in &shapes {
            for catalog_location in &inventory.locations {
                let alias = &catalog_location.alias;
                let location = &catalog_location.location;
                if location.region != self.settings.region
                    || !inventory.offered.contains(&(
                        shape.machine_type.clone(),
                        location.availability_zone.clone(),
                    ))
                {
                    *exclusion_counts
                        .entry(
                            "machine type is not offered in the configured availability zone"
                                .to_owned(),
                        )
                        .or_default() += 1;
                    continue;
                }

                let requested_throughput =
                    Decimal::from(profile.boot_disk.throughput_mib_s.unwrap_or(125).max(125));
                let requested_iops = profile.boot_disk.iops.unwrap_or(3000).max(3000);
                if requested_throughput > shape.ebs_throughput_mib_s
                    || requested_iops > shape.ebs_iops
                {
                    *exclusion_counts
                        .entry(
                            "machine type cannot meet requested boot-disk performance".to_owned(),
                        )
                        .or_default() += 1;
                    continue;
                }

                let subnet = &catalog_location.subnet;
                for purchasing in profile.purchasing_models.iter().filter(|model| {
                    profile
                        .administrator_limits
                        .allowed_purchasing
                        .contains(*model)
                        && matches!(model.as_str(), "spot" | "on_demand")
                }) {
                    let native_capacity_hint = (purchasing == "spot")
                        .then(|| inventory.spot_hints.get(alias).cloned())
                        .flatten();
                    let approved_extras = BTreeMap::from([
                        (
                            "subnet_id".to_owned(),
                            serde_json::json!(location.subnet_id),
                        ),
                        (
                            "security_group_ids".to_owned(),
                            serde_json::json!(location.security_group_ids),
                        ),
                        (
                            "spot".to_owned(),
                            serde_json::Value::Bool(purchasing == "spot"),
                        ),
                        ("volume_type".to_owned(), serde_json::json!("gp3")),
                        (
                            "volume_size".to_owned(),
                            serde_json::json!(profile.boot_disk.size_gib),
                        ),
                        (
                            "iops".to_owned(),
                            serde_json::json!(profile.boot_disk.iops.unwrap_or(3000).max(3000)),
                        ),
                        (
                            "throughput".to_owned(),
                            serde_json::json!(
                                profile.boot_disk.throughput_mib_s.unwrap_or(125).max(125)
                            ),
                        ),
                    ]);
                    let identity = format!(
                        "aws|{}|{}|{}|{}|{}|{}|{}",
                        self.settings.catalog_id,
                        self.settings.account_id,
                        location.region,
                        alias,
                        shape.machine_type,
                        image.image_id,
                        purchasing
                    );
                    let candidate = Candidate {
                        identity,
                        os: profile.os.clone(),
                        architecture: profile.architecture.clone(),
                        image: profile.image.clone(),
                        vcpu: shape.vcpu,
                        memory_gib: shape.memory_gib,
                        machine_type: shape.machine_type.clone(),
                        machine_family: shape.family.clone(),
                        cloud: "aws".to_owned(),
                        purchasing: purchasing.clone(),
                        account: self.settings.account_id.clone(),
                        location: alias.clone(),
                        gpu_vendor: shape.gpu.vendor.clone(),
                        gpu_model: shape.gpu.model.clone(),
                        gpu_count: shape.gpu.count,
                        gpu_memory_gib: shape.gpu.memory_gib,
                        boot_disk_class: profile.boot_disk.class.clone(),
                        boot_disk_gib: profile.boot_disk.size_gib,
                        boot_disk_iops: requested_iops,
                        boot_disk_throughput_mib_s: requested_throughput,
                        local_scratch_gib: shape.local_scratch_gib,
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
                        native_capacity_hint,
                    };

                    candidates.push(CatalogOffer {
                        candidate,
                        selection: ProviderSelection {
                            binding_id: self.settings.binding_id.clone(),
                            source_revision: self.settings.source_revision.clone(),
                            binary_sha256: self.settings.binary_sha256.clone(),
                            configuration_sha256: self.settings.configuration_sha256.clone(),
                            flavor: shape.machine_type.clone(),
                            image: image.image_id.clone(),
                            approved_extras,
                        },
                        launch_facts: ProviderLaunchFacts::Aws(AwsLaunchFacts {
                            availability_zone: subnet.availability_zone.clone(),
                            subnet_id: location.subnet_id.clone(),
                            map_public_ip_on_launch: subnet.map_public_ip_on_launch,
                            root_device_name: root_device_name.to_owned(),
                            root_volume_minimum_gib,
                            maximum_ebs_iops: shape.ebs_iops,
                            maximum_ebs_throughput_mib_s: shape.ebs_throughput_mib_s,
                        }),
                    });
                }
            }
        }

        Ok(candidates)
    }

    pub(super) async fn fetch_quote_image(
        &self,
        candidate: &Candidate,
        image: &AwsImage,
        deadline: Instant,
    ) -> Result<(&'static str, String, u32), CloudError> {
        let usage_operation = standard_usage_operation(&candidate.os).ok_or_else(|| {
            failure(
                "unsupported",
                "AWS quote OS has no supported usage operation",
                vec![],
            )
        })?;
        let actual = self
            .fetch_image_metadata(&image.image_id, "quote", deadline)
            .await?;
        if !image_metadata_matches(&actual, &candidate.os, &candidate.architecture) {
            return Err(failure(
                "unsupported",
                "AWS image metadata does not match its configured OS and architecture",
                vec![],
            ));
        }

        let root_device_name = actual
            .root_device_name()
            .ok_or_else(|| {
                failure(
                    "configuration",
                    "AWS AMI does not report a root device name",
                    vec![],
                )
            })?
            .to_owned();
        let root_volume_minimum_gib = actual
            .block_device_mappings()
            .iter()
            .find(|mapping| mapping.device_name() == Some(root_device_name.as_str()))
            .and_then(|mapping| mapping.ebs())
            .and_then(|ebs| ebs.volume_size())
            .and_then(|size| u32::try_from(size).ok())
            .filter(|size| *size > 0)
            .ok_or_else(|| {
                failure(
                    "configuration",
                    "AWS AMI root EBS volume has no valid minimum size",
                    vec![],
                )
            })?;
        Ok((usage_operation, root_device_name, root_volume_minimum_gib))
    }
}
