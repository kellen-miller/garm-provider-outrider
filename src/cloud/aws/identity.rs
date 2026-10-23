use super::{
    AwsBackend, BTreeMap, CloudError, Instant, InterruptionEvidence, PlacementIdentity,
    ProviderInstance, failure, sdk_failure, timeout_at,
};

impl AwsBackend {
    /// Read the retained Spot request's provider-side terminal reason and event time.
    ///
    /// # Errors
    /// Returns an error for identity mismatch, API failure, or an invalid response.
    pub async fn observe_interruption(
        &self,
        identity: &PlacementIdentity,
        deadline: Instant,
    ) -> Result<Vec<InterruptionEvidence>, CloudError> {
        let request_id = identity
            .dependency_ids
            .get("aws:spot-instance-request")
            .filter(|request_id| request_id.starts_with("sir-"))
            .ok_or_else(|| failure("ownership", "AWS Spot request identity is missing", vec![]))?;
        if identity.cloud != "aws"
            || identity.account_id != self.settings.account_id
            || identity.location != self.settings.region
            || !identity.resource_id.starts_with("i-")
        {
            return Err(failure(
                "ownership",
                "AWS interruption identity is outside the retained account and region",
                vec![],
            ));
        }
        let output = timeout_at(
            deadline,
            self.ec2
                .describe_spot_instance_requests()
                .spot_instance_request_ids(request_id)
                .send(),
        )
        .await
        .map_err(|_| failure("timeout", "AWS Spot request observation timed out", vec![]))?
        .map_err(|error| sdk_failure(&error, "AWS Spot request observation failed", vec![]))?;
        let mut source_revision = None;
        let mut evidence = None;
        for request in output
            .spot_instance_requests()
            .iter()
            .filter(|request| request.spot_instance_request_id() == Some(request_id.as_str()))
        {
            if request.instance_id() != Some(identity.resource_id.as_str()) {
                return Err(failure(
                    "ownership",
                    "AWS returned the retained Spot request for a different instance",
                    vec![],
                ));
            }
            let status = request.status();
            let revision = (
                status.and_then(|status| status.code().map(str::to_owned)),
                status.and_then(|status| status.update_time().map(ToString::to_string)),
                status.and_then(|status| status.message().map(str::to_owned)),
                request.state().map(|state| state.as_str().to_owned()),
            );
            if source_revision
                .as_ref()
                .is_some_and(|previous| previous != &revision)
            {
                return Err(failure(
                    "protocol",
                    "AWS returned conflicting status revisions for one Spot request",
                    vec![],
                ));
            }
            source_revision = Some(revision);
            let Some(status) = status else {
                continue;
            };
            let Some(code) = status.code() else {
                continue;
            };
            if !matches!(
                code,
                "instance-terminated-no-capacity" | "instance-terminated-by-price"
            ) {
                continue;
            }
            let Some(occurred_at) = status.update_time() else {
                return Err(failure(
                    "protocol",
                    "AWS Spot capacity interruption status has no provider update time",
                    vec![],
                ));
            };
            let reason_fields_json = serde_json::json!({
                "status_code": code,
                "status_message": status.message(),
                "request_state": request.state().map(|state| state.as_str()),
            })
            .to_string();
            evidence = Some(InterruptionEvidence {
                source_api: "ec2:DescribeSpotInstanceRequests".to_owned(),
                api_version: "2016-11-15".to_owned(),
                source_id: request_id.clone(),
                event_id: format!("{}@{}", request_id, occurred_at),
                occurred_at: occurred_at.to_string(),
                reason_fields_json,
            });
        }
        Ok(evidence.into_iter().collect())
    }

    pub(super) fn validate_identity(&self, identity: &PlacementIdentity) -> Result<(), CloudError> {
        if identity.cloud != "aws" || identity.account_id != self.settings.account_id {
            return Err(failure(
                "ownership",
                "AWS placement identity belongs to another account or cloud",
                vec![identity.clone()],
            ));
        }

        if identity.location != self.settings.region {
            return Err(failure(
                "ownership",
                "AWS placement identity belongs to another region",
                vec![identity.clone()],
            ));
        }

        Ok(())
    }

    pub(super) fn instance_from_model(
        &self,
        instance: &aws_sdk_ec2::types::Instance,
    ) -> Result<ProviderInstance, CloudError> {
        let resource_id = instance
            .instance_id()
            .ok_or_else(|| failure("protocol", "AWS instance has no resource ID", vec![]))?
            .to_owned();
        let controller_id = instance
            .tags()
            .iter()
            .find(|tag| tag.key() == Some("GARM_CONTROLLER_ID"))
            .and_then(|tag| tag.value())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                failure(
                    "ownership",
                    "AWS instance has no controller ownership tag",
                    vec![],
                )
            })?
            .to_owned();
        let has_pool_id = instance
            .tags()
            .iter()
            .find(|tag| tag.key() == Some("GARM_POOL_ID"))
            .and_then(|tag| tag.value())
            .is_some_and(|value| !value.is_empty());
        if !has_pool_id {
            return Err(failure(
                "ownership",
                "AWS instance has no pool ownership tag",
                vec![],
            ));
        }

        let availability_zone = instance
            .placement()
            .and_then(|placement| placement.availability_zone())
            .filter(|zone| !zone.is_empty())
            .ok_or_else(|| {
                failure(
                    "ownership",
                    "AWS instance has no availability zone placement fact",
                    vec![],
                )
            })?;
        let subnet_id = instance
            .subnet_id()
            .filter(|subnet| !subnet.is_empty())
            .ok_or_else(|| {
                failure(
                    "ownership",
                    "AWS instance has no subnet placement fact",
                    vec![],
                )
            })?;
        if !self.settings.locations.values().any(|location| {
            location.region == self.settings.region
                && location.subnet_id == subnet_id
                && location.availability_zone == availability_zone
        }) {
            return Err(failure(
                "ownership",
                "AWS instance subnet and availability zone do not match a configured placement",
                vec![],
            ));
        }

        let name = instance
            .tags()
            .iter()
            .find(|tag| tag.key() == Some("Name"))
            .and_then(|tag| tag.value())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                failure(
                    "ownership",
                    "AWS instance has no resource name ownership tag",
                    vec![],
                )
            })?
            .to_owned();
        let ownership_tags = aws_ownership_tags(instance);
        let dependency_ids = aws_instance_dependencies(instance);
        let identity = PlacementIdentity {
            controller_id,
            cloud: "aws".to_owned(),
            account_id: self.settings.account_id.clone(),
            location: self.settings.region.clone(),
            resource_id,
            host_id: None,
            dependency_ids,
        };

        Ok(ProviderInstance {
            identity,
            name,
            state: instance
                .state()
                .and_then(|state| state.name())
                .map(|state| state.as_str().to_owned())
                .unwrap_or_else(|| "unknown".to_owned()),
            ownership_tags,
        })
    }
}

pub(super) fn aws_ownership_tags(instance: &aws_sdk_ec2::types::Instance) -> Vec<(String, String)> {
    instance
        .tags()
        .iter()
        .filter_map(|tag| Some((tag.key()?.to_owned(), tag.value()?.to_owned())))
        .filter(|(key, _)| matches!(key.as_str(), "Name" | "GARM_CONTROLLER_ID" | "GARM_POOL_ID"))
        .collect()
}

pub(super) fn aws_instance_dependencies(
    instance: &aws_sdk_ec2::types::Instance,
) -> BTreeMap<String, String> {
    let mut dependencies = BTreeMap::new();
    for (index, interface_id) in instance
        .network_interfaces()
        .iter()
        .filter_map(|interface| interface.network_interface_id())
        .enumerate()
    {
        dependencies.insert(
            format!("network-interface:{index}"),
            interface_id.to_owned(),
        );
    }

    for volume_id in instance
        .block_device_mappings()
        .iter()
        .filter_map(|mapping| mapping.ebs().and_then(|ebs| ebs.volume_id()))
    {
        dependencies.insert(format!("volume:{volume_id}"), volume_id.to_owned());
    }

    if let Some(spot_request_id) = instance.spot_instance_request_id() {
        dependencies.insert(
            "aws:spot-instance-request".to_owned(),
            spot_request_id.to_owned(),
        );
    }

    dependencies
}
