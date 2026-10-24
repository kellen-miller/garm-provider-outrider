use super::{AllocationError, AllocationService, CatalogBackend, Duration, Instant, unix_now};
use crate::cloud::inventory::{CatalogInventory, CloudInventory, InventoryKind, InventoryReport};

impl AllocationService {
    /// Observe every connected current/retained catalog using its verified native credentials.
    /// Any unavailable scope fails the report; no partial report can imply absence.
    ///
    /// # Errors
    /// Returns an error for invalid configuration, absent catalogs, failed native
    /// collection, expired deadlines, or resource limits.
    pub async fn inventory(&self) -> Result<InventoryReport, AllocationError> {
        let ingress = self
            .config
            .ingress
            .as_ref()
            .ok_or(AllocationError::Configuration)?;
        let budget = ingress
            .allocation_timeout_seconds
            .min(u64::from(ingress.bootstrap_timeout_minutes).saturating_mul(60))
            .checked_sub(ingress.timeout_margin_seconds)
            .filter(|budget| *budget > 0)
            .ok_or(AllocationError::Configuration)?;
        if self.catalogs.is_empty() {
            return Err(AllocationError::Catalog);
        }

        let deadline = Instant::now() + Duration::from_secs(budget);
        let started_at_unix_seconds = unix_now();
        let mut catalogs = Vec::new();
        let mut count = 0usize;
        for catalog in &self.catalogs {
            let backend: &dyn CloudInventory = match &catalog.backend {
                CatalogBackend::Aws(backend) => backend,
                CatalogBackend::Azure(backend) => backend,
                CatalogBackend::Gcp(backend) => backend,
            };
            let mut collections = Vec::new();
            for kind in InventoryKind::ALL {
                let collection = backend
                    .inventory(kind, &ingress.controller_id, deadline)
                    .await
                    .map_err(|_| AllocationError::Catalog)?;
                count = count.saturating_add(collection.resources.len());
                if count > 10_000 {
                    return Err(AllocationError::Catalog);
                }

                collections.push(collection);
            }

            catalogs.push(CatalogInventory {
                catalog_id: catalog.catalog_id.clone(),
                collections,
            });
        }

        Ok(InventoryReport {
            controller_id: ingress.controller_id.clone(),
            started_at_unix_seconds,
            finished_at_unix_seconds: unix_now(),
            catalogs,
        })
    }
}
