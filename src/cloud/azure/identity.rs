use super::{
    AzureBackend, AzureResourceGraphPage, AzureSettings, AzureVm, BTreeMap, BTreeSet, CloudCatalog,
    CloudError, Instant, InterruptionEvidence, PlacementIdentity, ProviderInstance, failure,
};

impl AzureBackend {
    /// Read exact Resource Graph health annotations for one retained VM path.
    ///
    /// # Errors
    /// Returns an error for identity mismatch, API failure, truncation, or pagination failure.
    pub async fn observe_interruption(
        &self,
        identity: &PlacementIdentity,
        deadline: Instant,
    ) -> Result<Vec<InterruptionEvidence>, CloudError> {
        let retained_vm_id = identity
            .dependency_ids
            .get("vm_id")
            .filter(|vm_id| !vm_id.is_empty())
            .ok_or_else(|| failure("ownership", "Azure VM incarnation identity is missing"))?;
        if !identity.cloud.eq_ignore_ascii_case("azure")
            || !identity
                .account_id
                .eq_ignore_ascii_case(&self.settings.subscription_id)
            || !valid_vm_resource_id(&identity.resource_id, &self.settings.subscription_id)
            || !self
                .settings
                .locations
                .values()
                .any(|location| location.location.eq_ignore_ascii_case(&identity.location))
        {
            return Err(failure(
                "ownership",
                "Azure interruption identity is outside the retained subscription and location",
            ));
        }
        if let Some(current) = self.inspect(identity, deadline).await?
            && current
                .identity
                .dependency_ids
                .get("vm_id")
                .is_none_or(|current_vm_id| !current_vm_id.eq_ignore_ascii_case(retained_vm_id))
        {
            return Err(failure(
                "ownership",
                "Azure VM incarnation changed since the allocation was retained",
            ));
        }
        let url = format!(
            "{}/providers/Microsoft.ResourceGraph/resources?api-version=2024-04-01",
            self.settings.management_endpoint.trim_end_matches('/')
        );
        let expected_path = "/providers/Microsoft.ResourceGraph/resources";
        let mut skip_token: Option<String> = None;
        let mut seen_tokens = BTreeSet::new();
        let mut evidence = BTreeMap::new();
        let mut source_revisions = BTreeMap::new();
        loop {
            let mut options = serde_json::json!({"resultFormat":"objectArray", "$top":1000});
            if let Some(token) = &skip_token {
                options["$skipToken"] = serde_json::Value::String(token.clone());
            }
            let body = serde_json::json!({
                "subscriptions":[self.settings.subscription_id],
                "query":"HealthResources | where type =~ 'microsoft.resourcehealth/resourceannotations' | project id, name, type, subscriptionId, properties",
                "options":options
            });
            let response = self
                .post_management_json(&url, expected_path, &body, deadline)
                .await?;
            let page: AzureResourceGraphPage = serde_json::from_value(response)
                .map_err(|_| failure("protocol", "Azure Resource Graph response was invalid"))?;
            for annotation in page.data {
                let properties = annotation.properties;
                let revision_id = format!("{}@{}", annotation.id, properties.occurred_time);
                let revision_facts = serde_json::json!({
                    "subscription_id":&annotation.subscription_id,
                    "target_resource_id":&properties.target_resource_id,
                    "annotation_name":&properties.annotation_name,
                    "reason":&properties.reason,
                    "summary":&properties.summary,
                    "context":&properties.context,
                    "category":&properties.category,
                })
                .to_string();
                if source_revisions
                    .insert(revision_id.clone(), revision_facts.clone())
                    .is_some_and(|previous| previous != revision_facts)
                {
                    return Err(failure(
                        "protocol",
                        "Azure Resource Graph returned conflicting facts for one annotation revision",
                    ));
                }
                if !annotation
                    .subscription_id
                    .eq_ignore_ascii_case(&self.settings.subscription_id)
                    || !properties
                        .target_resource_id
                        .eq_ignore_ascii_case(&identity.resource_id)
                    || properties.annotation_name != "VirtualMachinePreempted"
                    || properties.context != "Platform-Initiated"
                    || properties.category != "Unplanned"
                {
                    continue;
                }
                if chrono::DateTime::parse_from_rfc3339(&properties.occurred_time).is_err() {
                    return Err(failure(
                        "protocol",
                        "Azure Resource Graph interruption annotation has no valid provider time",
                    ));
                }
                if annotation.id.trim().is_empty() {
                    return Err(failure(
                        "protocol",
                        "Azure Resource Graph interruption annotation has no source identifier",
                    ));
                }
                // ARG exposes no VM incarnation join and its annotation ID is not
                // documented as globally unique. Retain the raw ID while keying
                // this source revision by the provider's emission time.
                let observed = InterruptionEvidence {
                    source_api: "Microsoft.ResourceGraph/resources".to_owned(),
                    api_version: "2024-04-01".to_owned(),
                    source_id: annotation.id.clone(),
                    event_id: revision_id,
                    occurred_at: properties.occurred_time,
                    reason_fields_json: serde_json::json!({
                        "annotation_name":"VirtualMachinePreempted",
                        "context":"Platform-Initiated",
                        "category":"Unplanned",
                        "reason":properties.reason,
                        "summary":properties.summary,
                        "target_resource_id":properties.target_resource_id,
                    })
                    .to_string(),
                };
                if let Some(previous) = evidence.insert(observed.event_id.clone(), observed.clone())
                    && previous != observed
                {
                    return Err(failure(
                        "protocol",
                        "Azure Resource Graph returned conflicting annotations for one event identity",
                    ));
                }
            }
            let truncated = page
                .result_truncated
                .as_ref()
                .is_some_and(|value| value == "true" || value == true);
            let Some(next) = page.skip_token.filter(|token| !token.is_empty()) else {
                if truncated {
                    return Err(failure(
                        "pagination",
                        "Azure Resource Graph result was truncated without a continuation token",
                    ));
                }
                break;
            };
            if !seen_tokens.insert(next.clone()) || seen_tokens.len() > 1000 {
                return Err(failure(
                    "pagination",
                    "Azure Resource Graph pagination did not make bounded progress",
                ));
            }
            skip_token = Some(next);
        }
        Ok(evidence.into_values().collect())
    }
}

pub(super) fn provider_instance(vm: AzureVm, settings: &AzureSettings) -> ProviderInstance {
    let state = vm
        .properties
        .instance_view
        .as_ref()
        .and_then(|view| {
            view.statuses
                .iter()
                .find_map(|status| status.code.strip_prefix("PowerState/"))
        })
        .map(|state| match state.to_ascii_lowercase().as_str() {
            "running" => "running",
            "starting" => "pending_create",
            "stopping" => "stopping",
            "stopped" => "stopped",
            "deallocating" => "deallocating",
            "deallocated" => "deallocated",
            _ => "unknown",
        })
        .unwrap_or("unknown");
    ProviderInstance {
        identity: PlacementIdentity {
            controller_id: vm
                .tags
                .get("garm-controller-id")
                .cloned()
                .unwrap_or_default(),
            cloud: "azure".to_owned(),
            account_id: settings.subscription_id.clone(),
            location: vm.location,
            resource_id: vm.id,
            host_id: None,
            dependency_ids: vm
                .properties
                .vm_id
                .into_iter()
                .map(|id| ("vm_id".to_owned(), id))
                .collect(),
        },
        name: vm.name,
        state: state.to_owned(),
        ownership_tags: vm.tags.into_iter().collect(),
    }
}

pub(super) fn valid_vm_resource_id(value: &str, subscription: &str) -> bool {
    let value = value.to_ascii_lowercase();
    let expected = format!(
        "/subscriptions/{}/resourcegroups/",
        subscription.to_ascii_lowercase()
    );
    value.starts_with(&expected)
        && value.contains("/providers/microsoft.compute/virtualmachines/")
        && value.split('/').filter(|part| !part.is_empty()).count() == 8
}
