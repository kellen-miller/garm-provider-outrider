use super::billing::assemble_price_quote;

use super::{
    AwsBackend, AwsCatalogLocation, AwsQuotedPrices, BTreeMap, BTreeSet, CatalogOffer,
    CatalogQuery, CloudCatalog, CloudError, Decimal, Duration, Instant, OwnershipScope,
    PlacementIdentity, PriceQuote, ProviderInstance, ProviderLaunchFacts, failure, sdk_failure,
    timeout_at,
};

#[async_trait::async_trait]
impl CloudCatalog for AwsBackend {
    async fn catalog(
        &self,
        query: CatalogQuery,
        deadline: Instant,
    ) -> Result<crate::cloud::CatalogDiscovery, CloudError> {
        if !query.clouds.iter().any(|cloud| cloud == "aws") {
            return Ok(crate::cloud::CatalogDiscovery::default());
        }

        if (!query.accounts.is_empty() && !query.accounts.contains(&self.settings.account_id))
            || (!query.profile.accounts.is_empty()
                && !query.profile.accounts.contains(&self.settings.account_id))
        {
            return Ok(crate::cloud::CatalogDiscovery::default());
        }

        let profile = query.profile;
        let (image, root_device_name, root_volume_minimum_gib) =
            self.fetch_catalog_image(&profile, deadline).await?;
        let requested_locations = if query.locations.is_empty() {
            self.settings.locations.keys().cloned().collect::<Vec<_>>()
        } else {
            query.locations.clone()
        };

        let locations = requested_locations
            .iter()
            .filter(|location| profile.locations.is_empty() || profile.locations.contains(location))
            .map(|location| {
                Ok((
                    location.clone(),
                    self.configured_location(location)?.clone(),
                ))
            })
            .collect::<Result<Vec<_>, CloudError>>()?;
        let requested_types = profile.machine_types.get("aws");
        let mut catalog_locations = Vec::new();
        for (alias, location) in locations {
            let subnet = self.subnet_facts(&location, deadline).await?;
            catalog_locations.push(AwsCatalogLocation {
                alias,
                location,
                subnet,
            });
        }

        let mut inventory = self
            .fetch_catalog_inventory(requested_types, deadline)
            .await?;
        inventory.locations = catalog_locations;
        inventory.spot_hints = self
            .catalog_spot_hints(
                &inventory.machine_types,
                requested_types,
                &profile,
                deadline,
            )
            .await;
        let mut exclusion_counts = BTreeMap::new();
        let offers = self.catalog_candidates(
            &profile,
            image,
            &inventory,
            &root_device_name,
            root_volume_minimum_gib,
            &mut exclusion_counts,
        )?;
        Ok(crate::cloud::CatalogDiscovery {
            offers,
            exclusions: exclusion_counts
                .into_iter()
                .map(|(reason, observed_count)| crate::cloud::CatalogExclusion {
                    reason,
                    observed_count,
                })
                .collect(),
            queried: true,
        })
    }

    async fn quote(
        &self,
        offer: &CatalogOffer,
        _duration: Duration,
        _maximum_price_age: Duration,
        deadline: Instant,
    ) -> Result<PriceQuote, CloudError> {
        let candidate = &offer.candidate;
        let selection = &offer.selection;
        let ProviderLaunchFacts::Aws(launch_facts) = &offer.launch_facts else {
            return Err(failure(
                "configuration",
                "AWS quote received non-AWS launch facts",
                vec![],
            ));
        };
        if selection.binding_id != self.settings.binding_id
            || selection.source_revision != self.settings.source_revision
            || selection.binary_sha256 != self.settings.binary_sha256
            || selection.configuration_sha256 != self.settings.configuration_sha256
        {
            return Err(failure(
                "configuration",
                "AWS offer pins do not match the active central binding",
                vec![],
            ));
        }

        let location = self.configured_location(&candidate.location)?;
        if candidate.account != self.settings.account_id || location.region != self.settings.region
        {
            return Err(failure(
                "configuration",
                "AWS offer placement no longer matches its binding",
                vec![],
            ));
        }

        if self
            .settings
            .storage_classes
            .get(&candidate.boot_disk_class)
            .map(String::as_str)
            != Some("gp3")
        {
            return Err(failure(
                "unsupported",
                "AWS adapter currently supports gp3 boot disks only",
                vec![],
            ));
        }

        let image = self.configured_image(&candidate.image)?;
        if image.image_id != selection.image || candidate.machine_type != selection.flavor {
            return Err(failure(
                "configuration",
                "AWS offer flavor or image changed",
                vec![],
            ));
        }

        let Some(throughput) = selection
            .approved_extras
            .get("throughput")
            .and_then(serde_json::Value::as_u64)
            .filter(|value| *value >= 125)
            .filter(|value| Decimal::from(*value) <= candidate.boot_disk_throughput_mib_s)
        else {
            return Err(failure(
                "unsupported",
                "AWS child provider accepts only integral EBS throughput extras",
                vec![],
            ));
        };

        let subnet = self.subnet_facts(location, deadline).await?;
        if launch_facts.availability_zone != location.availability_zone
            || launch_facts.subnet_id != location.subnet_id
            || launch_facts.map_public_ip_on_launch != subnet.map_public_ip_on_launch
            || subnet.availability_zone != location.availability_zone
        {
            return Err(failure(
                "configuration",
                "AWS offer subnet facts changed",
                vec![],
            ));
        }

        let (usage_operation, root_device_name, root_volume_minimum_gib) =
            self.fetch_quote_image(candidate, image, deadline).await?;
        let expected_extras = BTreeMap::from([
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
                serde_json::Value::Bool(candidate.purchasing == "spot"),
            ),
            ("volume_type".to_owned(), serde_json::json!("gp3")),
            (
                "volume_size".to_owned(),
                serde_json::json!(candidate.boot_disk_gib),
            ),
            (
                "iops".to_owned(),
                serde_json::json!(candidate.boot_disk_iops),
            ),
            ("throughput".to_owned(), serde_json::json!(throughput)),
        ]);
        if launch_facts.root_device_name != root_device_name
            || launch_facts.root_volume_minimum_gib != root_volume_minimum_gib
            || candidate.boot_disk_gib < root_volume_minimum_gib
            || candidate.boot_disk_iops > launch_facts.maximum_ebs_iops
            || candidate.boot_disk_throughput_mib_s > launch_facts.maximum_ebs_throughput_mib_s
            || Decimal::from(throughput) != candidate.boot_disk_throughput_mib_s
            || selection.approved_extras != expected_extras
        {
            return Err(failure(
                "configuration",
                "AWS offer extras or root-device facts changed",
                vec![],
            ));
        }

        let observed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|value| value.as_secs());
        let compute = self
            .fetch_compute_quote(
                candidate,
                &selection.flavor,
                location,
                usage_operation,
                deadline,
            )
            .await
            .map_err(CloudError::price_unavailable)?;
        let disk = self
            .fetch_disk_quote(candidate, throughput, deadline)
            .await
            .map_err(CloudError::price_unavailable)?;
        let (public_rate, public_effective_at, mut provider_request_ids) = self
            .fetch_public_address_quote(subnet.map_public_ip_on_launch, deadline)
            .await
            .map_err(CloudError::price_unavailable)?;
        provider_request_ids.extend(compute.request_ids.iter().cloned());
        provider_request_ids.extend(disk.request_ids.iter().cloned());
        let prices = AwsQuotedPrices {
            compute,
            disk,
            public_rate,
            public_effective_at,
            provider_request_ids,
        };

        Ok(assemble_price_quote(candidate, image, prices, observed_at))
    }

    async fn inspect(
        &self,
        identity: &PlacementIdentity,
        deadline: Instant,
    ) -> Result<Option<ProviderInstance>, CloudError> {
        self.validate_identity(identity)?;
        if identity.resource_id.starts_with("client-token:") {
            return Err(failure(
                "ownership",
                "AWS placement identity requires a native instance ID",
                vec![identity.clone()],
            ));
        }
        let mut describe = self.ec2.describe_instances();
        describe = describe.instance_ids(&identity.resource_id);

        let result = timeout_at(deadline, describe.send()).await.map_err(|_| {
            failure(
                "timeout",
                "AWS instance inspection timed out",
                vec![identity.clone()],
            )
        })?;
        let output = match result {
            Ok(output) => output,
            Err(error)
                if error.as_service_error().is_some_and(|service| {
                    service.meta().code() == Some("InvalidInstanceID.NotFound")
                }) =>
            {
                return Ok(None);
            }

            Err(error) => {
                return Err(sdk_failure(
                    &error,
                    "AWS instance inspection failed",
                    vec![identity.clone()],
                ));
            }
        };

        let mut provider_instance = None;
        for reservation in output.reservations() {
            for instance in reservation.instances() {
                if instance.instance_id() != Some(identity.resource_id.as_str()) {
                    continue;
                }

                let provider = self.instance_from_model(instance)?;
                if reservation
                    .owner_id()
                    .is_some_and(|owner_id| owner_id != self.settings.account_id)
                {
                    return Err(failure(
                        "ownership",
                        "AWS instance reservation belongs to another account",
                        vec![provider.identity],
                    ));
                }
                provider_instance = Some(provider);
                break;
            }
            if provider_instance.is_some() {
                break;
            }
        }
        if provider_instance.as_ref().is_some_and(|instance| {
            instance.identity.controller_id != identity.controller_id
                || instance.identity.location != identity.location
        }) {
            return Err(failure(
                "ownership",
                "AWS instance is not tagged for this controller and placement",
                vec![identity.clone()],
            ));
        }

        Ok(provider_instance)
    }

    async fn list(
        &self,
        scope: &OwnershipScope,
        deadline: Instant,
    ) -> Result<Vec<ProviderInstance>, CloudError> {
        if scope.controller_id.is_empty()
            || scope.resource_name.as_deref().is_some_and(str::is_empty)
        {
            return Err(failure(
                "configuration",
                "AWS ownership scope requires a controller and non-empty optional name",
                vec![],
            ));
        }

        let mut instances = Vec::new();
        let mut seen_ids = BTreeSet::new();
        let mut seen_tokens = BTreeSet::new();
        let mut token = None;
        loop {
            let describe = self.ec2.describe_instances().filters(
                aws_sdk_ec2::types::Filter::builder()
                    .name("tag:GARM_CONTROLLER_ID")
                    .values(scope.controller_id.clone())
                    .build(),
            );
            let describe = if let Some(resource_name) = &scope.resource_name {
                describe.filters(
                    aws_sdk_ec2::types::Filter::builder()
                        .name("tag:Name")
                        .values(resource_name)
                        .build(),
                )
            } else {
                describe
            };
            let output = timeout_at(
                deadline,
                Box::pin(describe.set_next_token(token.clone()).send()),
            )
            .await
            .map_err(|_| failure("timeout", "AWS instance listing timed out", vec![]))?
            .map_err(|error| sdk_failure(&error, "AWS instance listing failed", vec![]))?;
            for reservation in output.reservations() {
                for instance in reservation.instances() {
                    let owner = instance
                        .tags()
                        .iter()
                        .find(|tag| tag.key() == Some("GARM_CONTROLLER_ID"))
                        .and_then(|tag| tag.value())
                        .filter(|owner| !owner.is_empty());
                    if owner.is_some_and(|owner| owner != scope.controller_id) {
                        continue;
                    }

                    let provider = self.instance_from_model(instance)?;
                    if reservation
                        .owner_id()
                        .is_some_and(|owner_id| owner_id != self.settings.account_id)
                    {
                        return Err(failure(
                            "ownership",
                            "AWS instance reservation belongs to another account",
                            vec![provider.identity],
                        ));
                    }

                    if scope
                        .resource_name
                        .as_ref()
                        .is_none_or(|resource_name| &provider.name == resource_name)
                        && seen_ids.insert(provider.identity.resource_id.clone())
                    {
                        instances.push(provider);
                    }
                }
            }

            let Some(next_token) = output.next_token().map(str::to_owned) else {
                break;
            };
            if !seen_tokens.insert(next_token.clone()) {
                return Err(failure(
                    "protocol",
                    "AWS instance listing repeated a pagination token",
                    vec![],
                ));
            }
            token = Some(next_token);
        }

        Ok(instances)
    }
}
