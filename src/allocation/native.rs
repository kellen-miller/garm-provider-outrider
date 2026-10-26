use super::catalog::binding_pin_key;
use super::selection::{binding_capability, selection_matches_binding};
use super::{
    AllocationError, AllocationRecord, AllocationService, BTreeSet, CatalogOffer, Duration,
    HashMap, HashSet, Instant, NativeIdentityLookup, NativeIdentityResolution, OwnershipScope,
    PlacementIdentity, ProviderBinding, ProviderRequest, ProviderRequirements, ProviderResult,
    Value,
};
use crate::journal::native_identity_key;
use crate::policy::ProviderLaunchFacts;

pub(super) fn allocation_dispatch(
    allocation: &AllocationRecord,
) -> Result<(CatalogOffer, ProviderBinding, Option<PlacementIdentity>), AllocationError> {
    let offer: CatalogOffer = serde_json::from_str(
        allocation
            .selected_offer_json
            .as_deref()
            .ok_or(AllocationError::Ambiguous)?,
    )
    .map_err(|_| AllocationError::Journal)?;
    let binding: ProviderBinding = serde_json::from_str(
        allocation
            .binding_json
            .as_deref()
            .ok_or(AllocationError::Ambiguous)?,
    )
    .map_err(|_| AllocationError::Journal)?;
    if !selection_matches_binding(&offer.selection, &binding) {
        return Err(AllocationError::Journal);
    }
    let identity = allocation
        .native_identity_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|_| AllocationError::Journal)?;
    Ok((offer, binding, identity))
}

impl AllocationService {
    pub(super) async fn resolve_native_identity(
        &self,
        lookup: NativeIdentityLookup<'_>,
    ) -> Result<NativeIdentityResolution, crate::cloud::CloudError> {
        let NativeIdentityLookup {
            controller_id,
            pool_id,
            offer,
            configuration_fingerprint,
            runner_name,
            child_instance_id,
            deadline,
        } = lookup;
        let scope = OwnershipScope {
            controller_id: controller_id.to_owned(),
            resource_name: Some(runner_name.to_owned()),
        };
        let mut resolution = NativeIdentityResolution::default();
        for catalog in self.catalogs.iter().filter(|catalog| {
            catalog.configuration_fingerprint == configuration_fingerprint
                && catalog.binding_id == offer.selection.binding_id
        }) {
            for observed in Box::pin(catalog.list(&scope, deadline)).await? {
                resolution.listed += 1;
                if observed.name != runner_name {
                    continue;
                }

                resolution.same_name += 1;
                if observed.identity.controller_id != controller_id {
                    resolution.first_rejection.get_or_insert("controller");
                    continue;
                }

                let pool_tag = match observed.identity.cloud.as_str() {
                    "aws" => "GARM_POOL_ID",
                    "azure" => "garm-pool-id",
                    "gcp" => "garmpoolid",
                    _ => continue,
                };
                let rejection = if !observed
                    .ownership_tags
                    .iter()
                    .any(|(key, value)| key == pool_tag && value == pool_id)
                {
                    Some("pool-tag")
                } else if !catalog.supports_identity(&observed.identity) {
                    Some("identity-scope")
                } else if child_instance_id
                    .is_some_and(|child_id| !child_locator_matches(&observed, child_id))
                {
                    Some("locator")
                } else {
                    None
                };
                if let Some(reason) = rejection {
                    resolution.first_rejection.get_or_insert(reason);
                    continue;
                }

                if resolution
                    .identity
                    .as_ref()
                    .is_some_and(|previous: &PlacementIdentity| {
                        previous.cloud != observed.identity.cloud
                            || previous.account_id != observed.identity.account_id
                            || previous.location != observed.identity.location
                            || previous.resource_id != observed.identity.resource_id
                    })
                {
                    return Err(crate::cloud::CloudError::Unavailable(
                        "native runner identity is ambiguous",
                    ));
                }
                resolution.identity = Some(observed.identity);
            }
        }
        Ok(resolution)
    }

    pub(super) async fn current_native_identity(
        &self,
        allocation: &AllocationRecord,
        offer: &CatalogOffer,
        child_instance_id: Option<&str>,
        deadline: Instant,
    ) -> Result<Option<PlacementIdentity>, AllocationError> {
        let observed = self
            .resolve_native_identity(NativeIdentityLookup {
                controller_id: &allocation.controller_id,
                pool_id: &allocation.pool_id,
                offer,
                configuration_fingerprint: allocation
                    .catalog_fingerprint
                    .as_deref()
                    .ok_or(AllocationError::Ambiguous)?,
                runner_name: &allocation.runner_name,
                child_instance_id,
                deadline,
            })
            .await
            .map_err(|_| AllocationError::Catalog)?
            .identity;
        if let (Some(stored), Some(observed)) = (
            allocation.native_identity_json.as_deref(),
            observed.as_ref(),
        ) {
            let stored: PlacementIdentity =
                serde_json::from_str(stored).map_err(|_| AllocationError::Journal)?;
            if &stored != observed {
                return Err(AllocationError::Ambiguous);
            }
        }
        Ok(observed)
    }

    pub(super) fn native_account_scope(
        &self,
        offer: &CatalogOffer,
        configuration_fingerprint: &str,
    ) -> Result<String, AllocationError> {
        let catalog = self
            .catalogs
            .iter()
            .find(|catalog| {
                catalog.configuration_fingerprint == configuration_fingerprint
                    && catalog.binding_id == offer.selection.binding_id
            })
            .ok_or(AllocationError::Configuration)?;
        canonical_native_account_scope(&catalog.cloud, &catalog.account_id)
    }

    pub(super) fn mark_ambiguous(
        &self,
        allocation: &AllocationRecord,
        attempt: u32,
        detail: &str,
    ) -> Result<(), AllocationError> {
        self.journal
            .finish_child_attempt(&allocation.allocation_id, attempt, "ambiguous", detail)
            .map_err(|_| AllocationError::Journal)?;
        self.journal
            .mark_allocation_ambiguous(&allocation.allocation_id, detail)
            .map_err(|_| AllocationError::Journal)
    }

    pub(super) fn scoped_allocation(
        &self,
        request: &ProviderRequest,
    ) -> Result<AllocationRecord, AllocationError> {
        let key = request
            .instance_id
            .as_deref()
            .ok_or(AllocationError::Conflict)?;
        let allocation = self
            .journal
            .allocation(&request.controller_id, key)
            .map_err(|_| AllocationError::Journal)?
            .ok_or(AllocationError::Provider)?;
        if request
            .pool_id
            .as_deref()
            .is_some_and(|pool_id| pool_id != allocation.pool_id)
        {
            return Err(AllocationError::Conflict);
        }

        Ok(allocation)
    }

    pub(super) async fn get(&self, request: ProviderRequest) -> Result<Value, AllocationError> {
        let allocation = self.scoped_allocation(&request)?;
        if !matches!(allocation.state.as_str(), "created" | "deleting") {
            return Err(AllocationError::Ambiguous);
        }

        let (offer, binding, identity) = allocation_dispatch(&allocation)?;
        let child_id = stored_child_instance_id(&allocation)?;
        let deadline = Instant::now() + Duration::from_secs(self.provider_timeout_seconds()?);
        let current_identity = self
            .current_native_identity(&allocation, &offer, Some(&child_id), deadline)
            .await?;
        if current_identity.is_none() || current_identity.as_ref() != identity.as_ref() {
            return Err(AllocationError::Ambiguous);
        }
        let result = self
            .invoke_allocation(
                &allocation,
                &offer,
                &binding,
                &child_id,
                "GetInstance",
                deadline,
            )
            .await?;
        let ProviderResult::Instance(mut instance) = result else {
            return Err(AllocationError::Ambiguous);
        };
        instance.provider_fault = None;
        instance.provider_id = allocation.allocation_id;
        serde_json::to_value(instance).map_err(|_| AllocationError::Journal)
    }

    pub(super) async fn list(&self, request: ProviderRequest) -> Result<Value, AllocationError> {
        let pool_id = request
            .pool_id
            .as_deref()
            .ok_or(AllocationError::Conflict)?;
        let allocations = self
            .journal
            .allocations_for_pool(&request.controller_id, pool_id)
            .map_err(|_| AllocationError::Journal)?;
        let deadline = Instant::now() + Duration::from_secs(self.provider_timeout_seconds()?);
        let mut relevant_bindings = self.provider_bindings.values().cloned().collect::<Vec<_>>();
        for allocation in &allocations {
            if allocation.state == "deleted"
                || (allocation.state == "no_candidate" && allocation.dispatched_at.is_none())
            {
                continue;
            }
            if let Some(json) = allocation.binding_json.as_deref() {
                let binding: ProviderBinding =
                    serde_json::from_str(json).map_err(|_| AllocationError::Journal)?;
                if !relevant_bindings.contains(&binding) {
                    relevant_bindings.push(binding);
                }
            }
        }
        relevant_bindings.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| left.source_revision.cmp(&right.source_revision))
                .then_with(|| left.executable_sha256.cmp(&right.executable_sha256))
                .then_with(|| left.configuration_sha256.cmp(&right.configuration_sha256))
        });

        let mut observed = HashMap::new();
        for binding in &relevant_bindings {
            let (os, architecture, purchasing) = binding_capability(binding)?;
            let requirements = ProviderRequirements {
                os,
                architecture,
                purchasing,
            };
            let child_request = ProviderRequest {
                command: "ListInstances".to_owned(),
                interface_version: crate::providers::GARM_INTERFACE_VERSION.to_owned(),
                controller_id: request.controller_id.clone(),
                pool_id: Some(pool_id.to_owned()),
                instance_id: None,
                bootstrap: None,
                pool_extra_specs: None,
            };
            match self
                .invoke_pinned(binding, &child_request, &requirements, deadline)
                .await
            {
                Ok(ProviderResult::Instances(instances)) => {
                    for instance in instances {
                        let native = (binding_pin_key(binding), instance.provider_id.clone());
                        observed.insert(native, (binding.clone(), instance));
                    }
                }
                Ok(_) | Err(_) => return Err(AllocationError::Catalog),
            }
        }

        let mut results = Vec::new();
        let mut returned_native = HashSet::new();
        let mut attributed_children = HashSet::new();
        for allocation in &allocations {
            if allocation.state == "deleted"
                || (allocation.state == "no_candidate" && allocation.dispatched_at.is_none())
            {
                continue;
            }

            if !matches!(allocation.state.as_str(), "created" | "deleting") {
                return Err(AllocationError::Ambiguous);
            }

            let (offer, binding, identity) = allocation_dispatch(allocation)?;
            let identity = identity.ok_or(AllocationError::Ambiguous)?;
            let binding_key = binding_pin_key(&binding);
            let child_id = stored_child_instance_id(allocation)?;
            let current = self
                .current_native_identity(allocation, &offer, Some(&child_id), deadline)
                .await?;
            let child_instance = observed.get(&(binding_key.clone(), child_id.clone()));
            let mut instance = if let Some((_, child_instance)) = child_instance {
                if current.as_ref() != Some(&identity) {
                    return Err(AllocationError::Ambiguous);
                }

                attributed_children.insert((binding_key, child_id));
                child_instance.clone()
            } else {
                // Ownership-filtered inventory omission is insufficient: inspect
                // the retained native ID before deciding whether it is absent.
                let inspected = self
                    .inspect_allocation_identity(allocation, &identity, deadline)
                    .await?;
                if inspected.is_none() && current.is_some() {
                    return Err(AllocationError::Ambiguous);
                }

                let Some(native) = inspected else {
                    continue;
                };
                let mut instance: crate::garm::ProviderInstance = serde_json::from_str(
                    allocation
                        .provider_instance_json
                        .as_deref()
                        .ok_or(AllocationError::Journal)?,
                )
                .map_err(|_| AllocationError::Journal)?;
                instance.status = native.state;
                instance
            };
            if !returned_native.insert(native_identity_key(&identity)) {
                continue;
            }

            instance.provider_id = allocation.allocation_id.clone();
            instance.provider_fault = None;
            results.push(serde_json::to_value(instance).map_err(|_| AllocationError::Journal)?);
        }

        for (provider_pin, resource_id) in observed.keys() {
            if !attributed_children.contains(&(provider_pin.clone(), resource_id.clone())) {
                eprintln!("outrider orphan candidate observed in a pool-tagged provider list");
            }
        }

        serde_json::to_value(results).map_err(|_| AllocationError::Journal)
    }
}

pub(super) fn native_pool_matches(
    instance: &crate::cloud::ProviderInstance,
    pool_id: &str,
) -> bool {
    let tag = match instance.identity.cloud.as_str() {
        "aws" => "GARM_POOL_ID",
        "azure" => "garm-pool-id",
        "gcp" => "garmpoolid",
        _ => return false,
    };
    instance
        .ownership_tags
        .iter()
        .any(|(key, value)| key == tag && value == pool_id)
}
pub(super) fn child_locator_matches(
    instance: &crate::cloud::ProviderInstance,
    child_id: &str,
) -> bool {
    match instance.identity.cloud.as_str() {
        "aws" => instance.identity.resource_id == child_id,
        "azure" => instance.name == child_id,
        "gcp" => instance
            .identity
            .dependency_ids
            .get("instance_name")
            .is_some_and(|name| {
                child_id == name || child_id == format!("{}/{}", instance.identity.location, name)
            }),
        _ => false,
    }
}
pub(super) fn stored_child_instance_id(
    allocation: &AllocationRecord,
) -> Result<String, AllocationError> {
    let instance: crate::garm::ProviderInstance = serde_json::from_str(
        allocation
            .provider_instance_json
            .as_deref()
            .ok_or(AllocationError::Ambiguous)?,
    )
    .map_err(|_| AllocationError::Journal)?;
    if instance.provider_id.is_empty() {
        return Err(AllocationError::Ambiguous);
    }
    Ok(instance.provider_id)
}
pub(super) fn stored_provider_instance(
    allocation: &AllocationRecord,
) -> Result<Value, AllocationError> {
    let mut instance: crate::garm::ProviderInstance = serde_json::from_str(
        allocation
            .provider_instance_json
            .as_deref()
            .ok_or(AllocationError::Ambiguous)?,
    )
    .map_err(|_| AllocationError::Journal)?;
    instance.provider_id = allocation.allocation_id.clone();
    instance.provider_fault = None;
    serde_json::to_value(instance).map_err(|_| AllocationError::Journal)
}
/// Return the concurrency and accounting key for one physical cloud account.
///
/// # Errors
/// Returns an error for unsupported clouds or an empty native account ID.
pub fn canonical_native_account_scope(
    cloud: &str,
    account_id: &str,
) -> Result<String, AllocationError> {
    if account_id.is_empty() {
        return Err(AllocationError::Configuration);
    }
    let account = match cloud {
        "aws" | "gcp" => account_id.to_owned(),
        "azure" => account_id.to_ascii_lowercase(),
        _ => return Err(AllocationError::Configuration),
    };
    Ok(format!("{cloud}:{account}"))
}

pub(super) fn azure_history_has_unique_incarnation(
    history: &[AllocationRecord],
    allocation: &AllocationRecord,
    identity: &PlacementIdentity,
) -> bool {
    let Some(vm_id) = identity
        .dependency_ids
        .get("vm_id")
        .filter(|vm_id| !vm_id.is_empty())
    else {
        return false;
    };
    let Ok(account_scope) = canonical_native_account_scope("azure", &identity.account_id) else {
        return false;
    };
    let mut path_generations = BTreeSet::new();
    for prior in history.iter().filter(|prior| prior.dispatched_at.is_some()) {
        let same_name_and_account = prior
            .runner_name
            .eq_ignore_ascii_case(&allocation.runner_name)
            && prior
                .account_id
                .as_deref()
                .is_some_and(|account| account.eq_ignore_ascii_case(&account_scope));
        let Some(offer_json) = prior.selected_offer_json.as_deref() else {
            if same_name_and_account {
                return false;
            }
            continue;
        };
        let Ok(prior_offer) = serde_json::from_str::<CatalogOffer>(offer_json) else {
            if same_name_and_account {
                return false;
            }
            continue;
        };
        let ProviderLaunchFacts::Azure(facts) = prior_offer.launch_facts else {
            continue;
        };
        let reserved_path = format!(
            "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.Compute/virtualMachines/{}",
            facts.subscription_id, prior.runner_name, prior.runner_name
        );
        if !reserved_path.eq_ignore_ascii_case(&identity.resource_id) {
            continue;
        }
        path_generations.insert(prior.allocation_id.as_str());
        let Some(retained) = prior
            .native_identity_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<PlacementIdentity>(json).ok())
        else {
            return false;
        };
        if !retained.cloud.eq_ignore_ascii_case("azure")
            || !retained
                .account_id
                .eq_ignore_ascii_case(&identity.account_id)
            || !retained.resource_id.eq_ignore_ascii_case(&reserved_path)
            || retained
                .dependency_ids
                .get("vm_id")
                .is_none_or(|retained_vm_id| !retained_vm_id.eq_ignore_ascii_case(vm_id))
        {
            return false;
        }
    }
    path_generations.len() == 1 && path_generations.contains(allocation.allocation_id.as_str())
}
