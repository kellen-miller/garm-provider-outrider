use super::cleanup::lifecycle_state_matches;
use super::native::{allocation_dispatch, stored_child_instance_id};
use super::selection::offer_requirements;
use super::{
    AllocationError, AllocationService, Duration, Instant, ProviderRequest, ProviderResult,
};

impl AllocationService {
    pub(super) async fn start_stop(&self, request: ProviderRequest) -> Result<(), AllocationError> {
        let allocation = self.scoped_allocation(&request)?;
        if allocation.state != "created" {
            return Err(AllocationError::Provider);
        }
        let admission = self
            .journal
            .begin_allocation_action(
                &allocation.controller_id,
                &allocation.allocation_id,
                &request.command,
            )
            .map_err(|_| AllocationError::Journal)?
            .ok_or(AllocationError::Provider)?;
        if !admission.acquired {
            return Err(AllocationError::Conflict);
        }
        let action_id = admission.action_id.ok_or(AllocationError::Journal)?;
        let allocation = admission.allocation;
        let deadline =
            Instant::now() + Duration::from_secs(self.provider_timeout_for_snapshot(&allocation)?);
        let (offer, binding, identity) = allocation_dispatch(&allocation)?;
        let identity = identity.ok_or(AllocationError::Ambiguous)?;
        let child_instance_id = stored_child_instance_id(&allocation)?;
        let current_identity = match self
            .current_native_identity(&allocation, &offer, Some(&child_instance_id), deadline)
            .await
        {
            Ok(identity) => identity,
            Err(error) => {
                self.journal
                    .finish_allocation_action(
                        action_id,
                        &request.command,
                        "ambiguous",
                        Some("native identity resolution was unavailable"),
                    )
                    .map_err(|_| AllocationError::Journal)?;
                return Err(error);
            }
        };
        if current_identity.as_ref() != Some(&identity) {
            self.journal
                .finish_allocation_action(
                    action_id,
                    &request.command,
                    "ambiguous",
                    Some("native resource incarnation changed"),
                )
                .map_err(|_| AllocationError::Journal)?;
            return Err(AllocationError::Ambiguous);
        }
        if !self
            .journal
            .claim_allocation_native_identity(&allocation.allocation_id, &identity)
            .map_err(|_| AllocationError::Journal)?
        {
            self.journal
                .finish_allocation_action(
                    action_id,
                    &request.command,
                    "ambiguous",
                    Some("native identity is owned by another allocation"),
                )
                .map_err(|_| AllocationError::Journal)?;
            return Err(AllocationError::Ambiguous);
        }
        let child_request = ProviderRequest {
            command: request.command.clone(),
            interface_version: crate::providers::GARM_INTERFACE_VERSION.to_owned(),
            controller_id: allocation.controller_id.clone(),
            pool_id: Some(allocation.pool_id.clone()),
            instance_id: Some(child_instance_id),
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
            self.journal
                .finish_allocation_action(
                    action_id,
                    &request.command,
                    "ambiguous",
                    Some("child lifecycle result was unresolved"),
                )
                .map_err(|_| AllocationError::Journal)?;
            return Err(AllocationError::Ambiguous);
        }
        let observed = match self
            .inspect_identity(
                &identity,
                allocation
                    .catalog_fingerprint
                    .as_deref()
                    .ok_or(AllocationError::Ambiguous)?,
                deadline,
            )
            .await
        {
            Ok(Some(observed)) => observed,
            Ok(None) => {
                self.journal
                    .finish_allocation_action(
                        action_id,
                        &request.command,
                        "ambiguous",
                        Some("native lifecycle result was not observed"),
                    )
                    .map_err(|_| AllocationError::Journal)?;
                return Err(AllocationError::Ambiguous);
            }
            Err(_) => {
                self.journal
                    .finish_allocation_action(
                        action_id,
                        &request.command,
                        "ambiguous",
                        Some("native lifecycle observation was unavailable"),
                    )
                    .map_err(|_| AllocationError::Journal)?;
                return Err(AllocationError::Catalog);
            }
        };
        if !lifecycle_state_matches(&offer.candidate.cloud, &request.command, &observed.state) {
            self.journal
                .finish_allocation_action(
                    action_id,
                    &request.command,
                    "ambiguous",
                    Some("native lifecycle state did not match the requested action"),
                )
                .map_err(|_| AllocationError::Journal)?;
            return Err(AllocationError::Ambiguous);
        }
        let current = self
            .journal
            .allocation(&allocation.controller_id, &allocation.allocation_id)
            .map_err(|_| AllocationError::Journal)?
            .ok_or(AllocationError::Journal)?;
        if current.tombstone {
            self.journal
                .finish_allocation_action(
                    action_id,
                    &request.command,
                    "ambiguous",
                    Some("delete was requested while lifecycle action was running"),
                )
                .map_err(|_| AllocationError::Journal)?;
            return Err(AllocationError::Ambiguous);
        }
        self.journal
            .finish_allocation_action(action_id, &request.command, "complete", None)
            .map_err(|_| AllocationError::Journal)?;
        Ok(())
    }

    pub(super) async fn remove_all(&self, request: ProviderRequest) -> Result<(), AllocationError> {
        use std::{future::Future, task::Poll};

        let deadline = Instant::now() + Duration::from_secs(self.provider_timeout_seconds()?);
        let allocations = self
            .journal
            .request_allocation_cleanup(&request.controller_id, request.pool_id.as_deref())
            .map_err(|_| AllocationError::Journal)?;
        let mut pending = allocations
            .iter()
            .filter(|allocation| allocation.state != "deleted")
            .map(|allocation| Some(Box::pin(self.delete_allocation(allocation, deadline))))
            .collect::<Vec<_>>();
        let mut unresolved = false;
        std::future::poll_fn(|context| {
            let mut active = false;
            for work in &mut pending {
                let Some(future) = work else {
                    continue;
                };
                if let Poll::Ready(result) = future.as_mut().poll(context) {
                    unresolved |= result.is_err();
                    *work = None;
                } else {
                    active = true;
                }
            }

            if active {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        if unresolved {
            Err(AllocationError::Ambiguous)
        } else {
            Ok(())
        }
    }
}
