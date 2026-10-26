use super::native::stored_child_instance_id;
use super::native::{allocation_dispatch, native_pool_matches};
use super::selection::offer_requirements;
use super::{
    AllocationError, AllocationRecord, AllocationService, BTreeMap, BTreeSet, BootstrapInstance,
    CatalogOffer, Duration, Instant, JournalError, PlacementIdentity, ProcessLimits,
    ProviderBinding, ProviderError, ProviderLaunchFacts, ProviderRegistry, ProviderRequest,
    ProviderRequirements, ProviderResult, SnapshotDocument,
};
use crate::journal::AllocationDeleteTarget;

impl AllocationService {
    pub(super) async fn delete(&self, request: ProviderRequest) -> Result<(), AllocationError> {
        let key = request
            .instance_id
            .as_deref()
            .ok_or(AllocationError::Conflict)?;
        let allocation = match self
            .journal
            .allocation_delete_target(&request.controller_id, key, request.pool_id.as_deref())
            .map_err(|error| match error {
                JournalError::AllocationPoolMismatch => AllocationError::Conflict,
                _ => AllocationError::Journal,
            })? {
            AllocationDeleteTarget::Owned(allocation) => *allocation,
            AllocationDeleteTarget::Absent => return Err(AllocationError::NotFound),
        };
        if allocation.state == "deleted" {
            return Ok(());
        }
        if allocation.dispatched_at.is_none() {
            let admission = self
                .journal
                .begin_allocation_action(
                    &allocation.controller_id,
                    &allocation.allocation_id,
                    "DeleteInstance",
                )
                .map_err(|_| AllocationError::Journal)?
                .ok_or(AllocationError::Provider)?;
            if admission.allocation.state == "deleted" {
                return Ok(());
            }
            if admission.acquired {
                self.journal
                    .finish_allocation_action(
                        admission.action_id.ok_or(AllocationError::Journal)?,
                        "DeleteInstance",
                        "ambiguous",
                        Some("delete raced a child dispatch; recovery will reconcile it"),
                    )
                    .map_err(|_| AllocationError::Journal)?;
            }
            return Err(AllocationError::Ambiguous);
        }

        let deadline =
            Instant::now() + Duration::from_secs(self.provider_timeout_for_snapshot(&allocation)?);
        Box::pin(self.delete_allocation(&allocation, deadline)).await
    }

    pub(super) async fn delete_allocation(
        &self,
        allocation: &AllocationRecord,
        deadline: Instant,
    ) -> Result<(), AllocationError> {
        let admission = self
            .journal
            .begin_allocation_action(
                &allocation.controller_id,
                &allocation.allocation_id,
                "DeleteInstance",
            )
            .map_err(|_| AllocationError::Journal)?
            .ok_or(AllocationError::Provider)?;
        if !admission.acquired {
            return if admission.allocation.state == "deleted" {
                Ok(())
            } else {
                Err(AllocationError::Ambiguous)
            };
        }

        let allocation = admission.allocation;
        let action_id = admission.action_id.ok_or(AllocationError::Journal)?;
        if allocation.state == "deleted" {
            return Ok(());
        }

        let result = async {
            if Instant::now() >= deadline {
                return Err(AllocationError::Ambiguous);
            }

            let (offer, binding, stored_identity) = allocation_dispatch(&allocation)?;
            let stored_child_id = allocation
                .provider_instance_json
                .as_ref()
                .map(|_| stored_child_instance_id(&allocation))
                .transpose()?;
            let terminal_create_returned = allocation.returned_at.is_some();
            let mut identity = self
                .current_native_identity(&allocation, &offer, stored_child_id.as_deref(), deadline)
                .await?;
            if let Some(stored) = stored_identity
                .as_ref()
                .filter(|identity| identity.cloud != "aws")
            {
                // Inspect the retained native ID as well as reusable name inventory.
                let inspected = self
                    .inspect_allocation_identity(&allocation, stored, deadline)
                    .await?;
                if identity.is_some() && inspected.is_none() {
                    return Err(AllocationError::Ambiguous);
                }

                identity = match inspected {
                    Some(inspected) => Some(inspected.identity),
                    None if identity.is_none() && terminal_create_returned => Some(stored.clone()),
                    None => None,
                };
            } else if identity.is_none() && terminal_create_returned && stored_identity.is_some() {
                if let Some(stored) = stored_identity.as_ref() {
                    let inspected = self
                        .inspect_allocation_identity(&allocation, stored, deadline)
                        .await?;
                    identity = Some(
                        inspected
                            .map(|inspected| inspected.identity)
                            .unwrap_or_else(|| stored.clone()),
                    );
                }
            } else if let ProviderLaunchFacts::Azure(facts) = &offer.launch_facts {
                // An absent VM does not prove its reserved resource group is empty.
                // Probe the exact reserved ARM path, including foreign replacements
                // hidden by ownership-filtered List, before qualified name cleanup.
                let resource_group = format!(
                    "/subscriptions/{}/resourceGroups/{}",
                    facts.subscription_id, allocation.runner_name
                );
                let reserved = PlacementIdentity {
                    controller_id: allocation.controller_id.clone(),
                    cloud: "azure".to_owned(),
                    account_id: facts.subscription_id.clone(),
                    location: facts.location.clone(),
                    resource_id: format!(
                        "{resource_group}/providers/Microsoft.Compute/virtualMachines/{}",
                        allocation.runner_name
                    ),
                    host_id: None,
                    dependency_ids: BTreeMap::new(),
                };
                let inspected = self
                    .inspect_identity(
                        &reserved,
                        allocation
                            .catalog_fingerprint
                            .as_deref()
                            .ok_or(AllocationError::Ambiguous)?,
                        deadline,
                    )
                    .await
                    .map_err(|_| AllocationError::Catalog)?;
                let invalid = inspected.as_ref().is_some_and(|inspected| {
                    inspected.name != allocation.runner_name
                        || !native_pool_matches(inspected, &allocation.pool_id)
                        || inspected
                            .identity
                            .dependency_ids
                            .get("vm_id")
                            .is_none_or(String::is_empty)
                        || identity
                            .as_ref()
                            .is_some_and(|identity| identity != &inspected.identity)
                });
                if invalid || (inspected.is_none() && identity.is_some()) {
                    return Err(AllocationError::Ambiguous);
                }

                identity = inspected.map(|inspected| inspected.identity);
            }

            if let Some(identity) = &identity {
                if !self
                    .journal
                    .claim_allocation_native_identity(&allocation.allocation_id, identity)
                    .map_err(|_| AllocationError::Journal)?
                {
                    return Err(AllocationError::Ambiguous);
                }
            } else if offer.candidate.cloud != "azure" || stored_identity.is_some() {
                return Err(AllocationError::Ambiguous);
            }

            let child_id = match (stored_child_id, offer.candidate.cloud.as_str()) {
                (Some(child_id), _) => child_id,
                (None, "azure") => allocation.runner_name.clone(),
                (None, "aws") => identity
                    .as_ref()
                    .ok_or(AllocationError::Ambiguous)?
                    .resource_id
                    .clone(),
                _ => {
                    self.resolve_child_name(&allocation, &offer, &binding, deadline)
                        .await?
                }
            };
            let child_request = ProviderRequest {
                command: "DeleteInstance".to_owned(),
                interface_version: crate::providers::GARM_INTERFACE_VERSION.to_owned(),
                controller_id: allocation.controller_id.clone(),
                pool_id: Some(allocation.pool_id.clone()),
                instance_id: Some(child_id.clone()),
                bootstrap: None,
                pool_extra_specs: None,
            };
            let result = self
                .invoke_pinned(
                    &binding,
                    &child_request,
                    &offer_requirements(&offer)?,
                    deadline,
                )
                .await;
            if !matches!(result, Ok(ProviderResult::Empty)) {
                return Err(AllocationError::Ambiguous);
            }

            if let Some(identity) = &identity
                && self
                    .inspect_allocation_identity(&allocation, identity, deadline)
                    .await?
                    .is_some()
            {
                return Err(AllocationError::Ambiguous);
            }

            // Re-read a positive Create return raced with this cleanup attempt.
            let current = self
                .journal
                .allocation(&allocation.controller_id, &allocation.allocation_id)
                .map_err(|_| AllocationError::Journal)?
                .ok_or(AllocationError::Journal)?;
            if admission.unresolved_create
                && (current.returned_at.is_none()
                    || stored_child_instance_id(&current).ok().as_deref()
                        != Some(child_id.as_str()))
            {
                return Err(AllocationError::Ambiguous);
            }

            if identity.is_none() {
                // VM-less Azure dependency cleanup cannot prove a generic Create absent.
                return Err(AllocationError::Ambiguous);
            }

            self.journal
                .set_allocation_state(&allocation.allocation_id, "deleted", None)
                .map_err(|_| AllocationError::Journal)?;
            Ok(())
        }
        .await;
        self.journal
            .finish_allocation_action(
                action_id,
                "DeleteInstance",
                if result.is_ok() {
                    "complete"
                } else {
                    "ambiguous"
                },
                result
                    .as_ref()
                    .err()
                    .map(|_| "guarded child cleanup remains unresolved"),
            )
            .map_err(|_| AllocationError::Journal)?;
        result
    }

    pub(super) async fn invoke_pinned(
        &self,
        binding: &ProviderBinding,
        request: &ProviderRequest,
        requirements: &ProviderRequirements,
        deadline: Instant,
    ) -> Result<ProviderResult, ProviderError> {
        if self.provider_bindings.get(&binding.id) == Some(binding) {
            self.provider_registry
                .invoke(
                    &binding.id,
                    request,
                    Some(requirements),
                    self.config.admission.as_ref(),
                    deadline,
                )
                .await
        } else {
            let registry = ProviderRegistry::new(
                vec![binding.clone()],
                self.process_environment.clone(),
                ProcessLimits::default(),
            )?;
            registry
                .invoke(
                    &binding.id,
                    request,
                    Some(requirements),
                    self.config.admission.as_ref(),
                    deadline,
                )
                .await
        }
    }

    pub(super) async fn invoke_allocation(
        &self,
        allocation: &AllocationRecord,
        offer: &CatalogOffer,
        binding: &ProviderBinding,
        instance_id: &str,
        command: &str,
        deadline: Instant,
    ) -> Result<ProviderResult, AllocationError> {
        let request = ProviderRequest {
            command: command.to_owned(),
            interface_version: crate::providers::GARM_INTERFACE_VERSION.to_owned(),
            controller_id: allocation.controller_id.clone(),
            pool_id: Some(allocation.pool_id.clone()),
            instance_id: Some(instance_id.to_owned()),
            bootstrap: None,
            pool_extra_specs: None,
        };
        self.invoke_pinned(binding, &request, &offer_requirements(offer)?, deadline)
            .await
            .map_err(|_| AllocationError::Provider)
    }

    pub(super) fn provider_timeout_seconds(&self) -> Result<u64, AllocationError> {
        self.config
            .control
            .as_ref()
            .map(|settings| settings.provider_timeout_seconds)
            .filter(|seconds| *seconds > 0)
            .ok_or(AllocationError::Configuration)
    }

    pub(super) fn provider_timeout_for_snapshot(
        &self,
        allocation: &AllocationRecord,
    ) -> Result<u64, AllocationError> {
        let snapshot = self
            .journal
            .pool_snapshot_by_id(&allocation.snapshot_id)
            .map_err(|_| AllocationError::Journal)?
            .ok_or(AllocationError::Snapshot)?;
        let document: SnapshotDocument =
            serde_json::from_str(&snapshot.config_json).map_err(|_| AllocationError::Snapshot)?;
        document
            .central
            .control
            .map(|settings| settings.provider_timeout_seconds)
            .filter(|seconds| *seconds > 0)
            .ok_or(AllocationError::Configuration)
    }

    pub(super) async fn inspect_allocation_identity(
        &self,
        allocation: &AllocationRecord,
        identity: &PlacementIdentity,
        deadline: Instant,
    ) -> Result<Option<crate::cloud::ProviderInstance>, AllocationError> {
        let observed = self
            .inspect_identity(
                identity,
                allocation
                    .catalog_fingerprint
                    .as_deref()
                    .ok_or(AllocationError::Ambiguous)?,
                deadline,
            )
            .await
            .map_err(|_| AllocationError::Catalog)?;
        if observed.as_ref().is_some_and(|observed| {
            observed.identity != *identity
                || observed.name != allocation.runner_name
                || !native_pool_matches(observed, &allocation.pool_id)
        }) {
            return Err(AllocationError::Ambiguous);
        }

        Ok(observed)
    }

    pub(super) async fn inspect_identity(
        &self,
        identity: &PlacementIdentity,
        configuration_fingerprint: &str,
        deadline: Instant,
    ) -> Result<Option<crate::cloud::ProviderInstance>, crate::cloud::CloudError> {
        let mut found = None;
        let mut inspected_scope = false;
        for catalog in &self.catalogs {
            if catalog.configuration_fingerprint != configuration_fingerprint
                || !catalog.supports_identity(identity)
            {
                continue;
            }
            inspected_scope = true;
            if let Some(instance) = catalog.inspect(identity, deadline).await? {
                if found.is_some() {
                    return Err(crate::cloud::CloudError::Unavailable(
                        "ambiguous cloud identity",
                    ));
                }
                found = Some(instance);
            }
        }
        if !inspected_scope {
            return Err(crate::cloud::CloudError::Unavailable(
                "historical cloud identity scope is unavailable",
            ));
        }
        Ok(found)
    }

    pub(super) async fn resolve_child_name(
        &self,
        allocation: &AllocationRecord,
        offer: &CatalogOffer,
        binding: &ProviderBinding,
        deadline: Instant,
    ) -> Result<String, AllocationError> {
        let child_request = ProviderRequest {
            command: "ListInstances".to_owned(),
            interface_version: crate::providers::GARM_INTERFACE_VERSION.to_owned(),
            controller_id: allocation.controller_id.clone(),
            pool_id: Some(allocation.pool_id.clone()),
            instance_id: None,
            bootstrap: None,
            pool_extra_specs: None,
        };
        let result = self
            .invoke_pinned(
                binding,
                &child_request,
                &offer_requirements(offer)?,
                deadline,
            )
            .await
            .map_err(|_| AllocationError::Ambiguous)?;
        let ProviderResult::Instances(instances) = result else {
            return Err(AllocationError::Ambiguous);
        };
        let matches = instances
            .into_iter()
            .filter(|instance| instance.name == allocation.runner_name)
            .collect::<Vec<_>>();
        if matches.len() != 1 || matches[0].provider_id.is_empty() {
            return Err(AllocationError::Ambiguous);
        }
        Ok(matches[0].provider_id.clone())
    }
}

pub(super) fn lifecycle_state_matches(cloud: &str, command: &str, state: &str) -> bool {
    let state = state.to_ascii_lowercase();
    match (command, cloud) {
        ("StartInstance", _) => state == "running",
        ("StopInstance", "aws") => state == "stopped",
        ("StopInstance", "azure") => matches!(state.as_str(), "stopped" | "deallocated"),
        ("StopInstance", "gcp") => state == "terminated",
        _ => false,
    }
}
pub(super) fn valid_bootstrap_labels(
    bootstrap: &BootstrapInstance,
    request_token: &str,
    controller_id: &str,
    pool_id: &str,
) -> bool {
    let expected = BTreeSet::from([
        request_token.to_owned(),
        format!("runner-pool-id={pool_id}"),
        format!("runner-controller-id={controller_id}"),
    ]);
    let actual = bootstrap.labels.iter().cloned().collect::<BTreeSet<_>>();
    if actual.len() != bootstrap.labels.len() || !expected.is_superset(&actual) {
        return false;
    }
    if bootstrap.jit_config_enabled && bootstrap.labels.is_empty() {
        return true;
    }
    actual == expected
}
