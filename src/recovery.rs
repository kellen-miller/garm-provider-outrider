//! Daemon-owned restart and periodic recovery orchestration.

use std::sync::Arc;

use thiserror::Error;

use crate::{
    allocation::{AllocationError, AllocationService},
    github::{IngressError, IngressService},
    journal::{Journal, JournalError},
};

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("recovery journal startup failed")]
    Journal(#[from] JournalError),
    #[error("one or more recovery tasks remain unresolved")]
    Pending,
}

/// Coordinates durable allocation, ingress, and pool recovery for one daemon owner.
pub struct RecoveryService {
    allocation: Arc<AllocationService>,
    ingress: Arc<IngressService>,
    journal: Arc<Journal>,
}

impl RecoveryService {
    /// Bind the required production services to the daemon's exclusive journal owner.
    #[must_use]
    pub fn new(
        allocation: Arc<AllocationService>,
        ingress: Arc<IngressService>,
        journal: Arc<Journal>,
    ) -> Self {
        Self {
            allocation,
            ingress,
            journal,
        }
    }

    /// Mark prior-process actions orphaned before serving new provider requests.
    ///
    /// # Errors
    /// Returns `RecoveryError::Journal` when durable startup recovery fails.
    pub fn startup(&self) -> Result<(), RecoveryError> {
        self.journal.recover_startup_allocations()?;
        Ok(())
    }

    /// Run one bounded recovery pass across allocations, webhook work, and pool drains.
    ///
    /// # Errors
    /// Returns `RecoveryError::Pending` if any independent recovery task remains unresolved.
    pub async fn iteration(&self, limit: usize) -> Result<usize, RecoveryError> {
        let mut recovered = 0;
        let mut pending = false;
        match self.allocation.observe_interruptions(limit).await {
            Ok(count) => recovered += count,
            Err(_) => pending = true,
        }
        match self.ingress.rerun_completed_interrupted_jobs(limit).await {
            Ok(count) => recovered += count,
            Err(_) => pending = true,
        }
        match self.allocation.recover_pending_cleanup(limit).await {
            Ok(count) => recovered += count,
            Err(AllocationError::Journal) => pending = true,
            Err(_) => pending = true,
        }
        match self
            .allocation
            .recover_orphaned_lifecycle_actions(limit)
            .await
        {
            Ok(count) => recovered += count,
            Err(_) => pending = true,
        }
        match self.ingress.process_pending(limit).await {
            Ok(count) => recovered += count,
            Err(IngressError::Journal | IngressError::Configuration) => pending = true,
            Err(_) => pending = true,
        }
        match self.ingress.drain_expired_pools(limit).await {
            Ok(count) => recovered += count,
            Err(_) => pending = true,
        }
        match self
            .journal
            .maintain_retention(self.allocation.history_window_seconds(), limit)
        {
            Ok(count) => recovered += count,
            Err(_) => pending = true,
        }
        if pending {
            Err(RecoveryError::Pending)
        } else {
            Ok(recovered)
        }
    }
}
