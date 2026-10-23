use super::connection::path_segment;
use super::{
    BTreeMap, CloudError, GcpBackend, GcpChildConfiguration, GcpInstance, GcpOperation,
    GcpSettings, Instant, InterruptionEvidence, PlacementIdentity, ProviderInstance, Url, failure,
};

impl GcpBackend {
    /// Read all pinned-zone operations and retain an exact completed preemption.
    ///
    /// # Errors
    /// Returns an error for identity mismatch, API failure, or incomplete pagination.
    pub async fn observe_interruption(
        &self,
        identity: &PlacementIdentity,
        deadline: Instant,
    ) -> Result<Vec<InterruptionEvidence>, CloudError> {
        let instance_name = identity
            .dependency_ids
            .get("instance_name")
            .filter(|name| !name.is_empty())
            .ok_or_else(|| failure("ownership", "GCP instance name identity is missing"))?;
        if identity.cloud != "gcp"
            || identity.account_id != self.settings.project_id
            || identity.location != self.settings.zone
            || identity.resource_id.parse::<u64>().is_err()
        {
            return Err(failure(
                "ownership",
                "GCP interruption identity is outside the retained project and zone",
            ));
        }
        let path = format!(
            "/compute/v1/projects/{}/zones/{}/operations",
            path_segment(&self.settings.project_id)?,
            path_segment(&self.settings.zone)?,
        );
        let (operations, _) = self
            .list_all::<GcpOperation>(&self.settings.compute_endpoint, &path, deadline)
            .await?;
        let mut source_revisions = BTreeMap::new();
        let expected_path = format!(
            "/compute/v1/projects/{}/zones/{}/instances/{}",
            self.settings.project_id, self.settings.zone, instance_name
        );
        let mut evidence = Vec::new();
        for operation in operations {
            let source_id = if operation.name.is_empty() {
                operation.id.as_str()
            } else {
                operation.name.as_str()
            };
            if source_id.trim().is_empty() {
                continue;
            }
            let revision_facts = serde_json::json!({
                "id":&operation.id,
                "operation_type":&operation.operation_type,
                "target_link":&operation.target_link,
                "target_id":&operation.target_id,
                "status":&operation.status,
                "start_time":&operation.start_time,
                "end_time":&operation.end_time,
                "error":&operation.error,
            })
            .to_string();
            if source_revisions
                .insert(source_id.to_owned(), revision_facts.clone())
                .is_some_and(|previous| previous != revision_facts)
            {
                return Err(failure(
                    "protocol",
                    "GCP zone operations returned conflicting facts for one operation revision",
                ));
            }
            let target = reqwest::Url::parse(&operation.target_link)
                .map_err(|_| failure("protocol", "GCP preemption target link was invalid"))?;
            let target_path_matches = matches!(
                target.host_str(),
                Some("www.googleapis.com" | "compute.googleapis.com")
            ) && target.path() == expected_path;
            if operation.operation_type != "compute.instances.preempted"
                || operation.status != "DONE"
                || operation.error.is_some()
                || operation.target_id != identity.resource_id
                || !target_path_matches
            {
                continue;
            }
            let Some(occurred_at) = operation
                .start_time
                .as_ref()
                .or(operation.end_time.as_ref())
            else {
                return Err(failure(
                    "protocol",
                    "GCP completed preemption operation has no provider time",
                ));
            };
            if chrono::DateTime::parse_from_rfc3339(occurred_at).is_err() {
                return Err(failure(
                    "protocol",
                    "GCP completed preemption operation has an invalid provider time",
                ));
            }
            let reason_fields_json = serde_json::json!({
                "operation_type": operation.operation_type,
                "status": operation.status,
                "target_id": operation.target_id,
                "target_link": operation.target_link,
            })
            .to_string();
            evidence.push(InterruptionEvidence {
                source_api: "compute.zoneOperations.list".to_owned(),
                api_version: "v1".to_owned(),
                source_id: if operation.name.is_empty() {
                    operation.id.clone()
                } else {
                    operation.name.clone()
                },
                event_id: if operation.name.is_empty() {
                    operation.id
                } else {
                    operation.name
                },
                occurred_at: occurred_at.clone(),
                reason_fields_json,
            });
        }
        Ok(evidence)
    }
}

pub(super) fn map_instance(
    settings: &GcpSettings,
    child: &GcpChildConfiguration,
    controller_id: String,
    instance: GcpInstance,
) -> Result<ProviderInstance, CloudError> {
    let zone_url = Url::parse(&instance.zone)
        .map_err(|_| failure("response", "GCP instance has an invalid native zone URL"))?;
    let resource = zone_url
        .path()
        .strip_prefix("/compute/v1/projects/")
        .ok_or_else(|| {
            failure(
                "response",
                "GCP instance zone is not a Compute project resource",
            )
        })?;
    let (project, zone_path) = resource.split_once("/zones/").ok_or_else(|| {
        failure(
            "response",
            "GCP instance zone is not a Compute zone resource",
        )
    })?;
    if zone_url.scheme() != "https"
        || zone_url.host_str() != Some("www.googleapis.com")
        || project != settings.project_id
        || zone_path.contains('/')
        || zone_path != settings.zone
    {
        return Err(failure(
            "response",
            "GCP instance response project or zone does not match the configured native scope",
        ));
    }
    if instance.id.is_empty()
        || !instance.id.bytes().all(|byte| byte.is_ascii_digit())
        || instance.name.is_empty()
    {
        return Err(failure(
            "response",
            "GCP instance has a malformed numeric ID or native name",
        ));
    }
    let metadata = instance
        .metadata
        .as_ref()
        .map(|metadata| {
            metadata
                .items
                .iter()
                .filter_map(|item| {
                    item.value
                        .as_ref()
                        .map(|value| (item.key.as_str(), value.as_str()))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let owner_controller = instance.labels.get("garmcontrollerid").map(String::as_str);
    let pool = instance.labels.get("garmpoolid").map(String::as_str);
    let runner_name = metadata.get("runner_name").copied();
    let controller_matches = owner_controller.is_some_and(|value| value == controller_id);
    if controller_matches
        && (pool.is_none_or(str::is_empty) || runner_name.is_none_or(str::is_empty))
    {
        return Err(failure(
            "ownership",
            "GCP instance has incomplete controller, pool, or runner-name ownership metadata",
        ));
    }
    let scoped_owner = controller_matches;
    let ownership_tags = if scoped_owner {
        vec![
            (
                "garmcontrollerid".to_owned(),
                owner_controller.unwrap_or_default().to_owned(),
            ),
            ("garmpoolid".to_owned(), pool.unwrap_or_default().to_owned()),
        ]
    } else {
        Vec::new()
    };
    let resource_id = instance.id.clone();
    Ok(ProviderInstance {
        identity: PlacementIdentity {
            controller_id,
            cloud: "gcp".to_owned(),
            account_id: settings.project_id.clone(),
            location: settings.zone.clone(),
            resource_id,
            host_id: None,
            dependency_ids: BTreeMap::from([
                ("network".to_owned(), child.network_id.clone()),
                ("subnetwork".to_owned(), child.subnetwork_id.clone()),
                ("instance_name".to_owned(), instance.name.clone()),
            ]),
        },
        name: runner_name.unwrap_or(&instance.name).to_owned(),
        state: instance.status,
        ownership_tags: if scoped_owner {
            ownership_tags
        } else {
            Vec::new()
        },
    })
}
