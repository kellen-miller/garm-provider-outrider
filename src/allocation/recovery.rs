use super::cleanup::lifecycle_state_matches;
use super::native::{allocation_dispatch, stored_child_instance_id};
use super::{AllocationError, AllocationService, Duration, Instant, Ordering};

impl AllocationService {
    /// Retry a bounded, rotating batch of durable allocation cleanup intents.
    ///
    /// # Errors
    /// Returns `AllocationError::Ambiguous` when any cleanup remains unresolved.
    pub async fn recover_pending_cleanup(&self, limit: usize) -> Result<usize, AllocationError> {
        use std::{future::Future, task::Poll};

        if limit == 0 {
            return Ok(0);
        }
        let allocations = self
            .journal
            .recoverable_allocations()
            .map_err(|_| AllocationError::Journal)?;
        if allocations.is_empty() {
            self.recovery_offset.store(0, Ordering::Relaxed);
            return Ok(0);
        }
        let count = limit.min(allocations.len());
        let start = self.recovery_offset.fetch_add(count, Ordering::Relaxed) % allocations.len();
        let deadline = Instant::now() + Duration::from_secs(self.provider_timeout_seconds()?);
        let mut failed = false;
        let mut recovered = 0;
        let mut pending = (0..count)
            .map(|index| {
                let allocation = &allocations[(start + index) % allocations.len()];
                Some(Box::pin(self.delete_allocation(allocation, deadline)))
            })
            .collect::<Vec<_>>();
        std::future::poll_fn(|context| {
            let mut active = false;
            for work in &mut pending {
                let Some(future) = work else {
                    continue;
                };
                if let Poll::Ready(result) = future.as_mut().poll(context) {
                    recovered += usize::from(result.is_ok());
                    failed |= result.is_err();
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
        if failed {
            Err(AllocationError::Ambiguous)
        } else {
            Ok(recovered)
        }
    }

    /// Reconcile orphaned Start and Stop actions against their retained native identity.
    ///
    /// # Errors
    /// Returns `AllocationError::Ambiguous` when any observed outcome remains uncertain.
    pub async fn recover_orphaned_lifecycle_actions(
        &self,
        limit: usize,
    ) -> Result<usize, AllocationError> {
        use std::{future::Future, task::Poll};

        if limit == 0 {
            return Ok(0);
        }
        let actions = self
            .journal
            .recoverable_lifecycle_actions()
            .map_err(|_| AllocationError::Journal)?;
        if actions.is_empty() {
            return Ok(0);
        }
        let count = limit.min(actions.len());
        let start = self
            .lifecycle_recovery_offset
            .fetch_add(count, Ordering::Relaxed)
            % actions.len();
        let deadline = Instant::now() + Duration::from_secs(self.provider_timeout_seconds()?);
        let mut recovered = 0;
        let mut unresolved = false;
        let mut pending = (0..count)
            .map(|index| {
                let action = &actions[(start + index) % actions.len()];
                Some(Box::pin(
                    self.reconcile_orphaned_lifecycle_action(action, deadline),
                ))
            })
            .collect::<Vec<_>>();
        std::future::poll_fn(|context| {
            let mut active = false;
            for work in &mut pending {
                let Some(future) = work else {
                    continue;
                };
                if let Poll::Ready(result) = future.as_mut().poll(context) {
                    recovered += usize::from(result.is_ok());
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
            Ok(recovered)
        }
    }

    pub(super) async fn reconcile_orphaned_lifecycle_action(
        &self,
        action: &crate::journal::AllocationActionRecord,
        deadline: Instant,
    ) -> Result<(), AllocationError> {
        let result = async {
            let allocation = self
                .journal
                .allocation_by_id(&action.allocation_id)
                .map_err(|_| AllocationError::Journal)?
                .ok_or(AllocationError::Journal)?;
            let (offer, _binding, identity) = allocation_dispatch(&allocation)?;
            let identity = identity.ok_or(AllocationError::Ambiguous)?;
            let child_id = stored_child_instance_id(&allocation)?;
            if self
                .current_native_identity(&allocation, &offer, Some(&child_id), deadline)
                .await?
                .as_ref()
                != Some(&identity)
            {
                return Err(AllocationError::Ambiguous);
            }
            let observed = self
                .inspect_allocation_identity(&allocation, &identity, deadline)
                .await?
                .ok_or(AllocationError::Ambiguous)?;
            if !lifecycle_state_matches(&offer.candidate.cloud, &action.command, &observed.state) {
                return Err(AllocationError::Ambiguous);
            }
            self.journal
                .finish_recovered_lifecycle_action(
                    action.action_id,
                    &action.command,
                    "complete",
                    None,
                )
                .map_err(|_| AllocationError::Journal)?;
            Ok(())
        }
        .await;
        if result.is_err() {
            self.journal
                .finish_recovered_lifecycle_action(
                    action.action_id,
                    &action.command,
                    "ambiguous",
                    Some("restart lifecycle outcome could not be verified"),
                )
                .map_err(|_| AllocationError::Journal)?;
        }
        result
    }
}
