use super::{IngressError, IngressService, PoolError, SnapshotDocument, now};

impl IngressService {
    /// Disable expired pools and delete them only after GARM reports no instances or active operations.
    ///
    /// # Errors
    /// Returns an error when validation, provenance, persistence, or a dependency request fails.
    pub async fn drain_expired_pools(&self, limit: usize) -> Result<usize, IngressError> {
        if limit == 0 {
            return Ok(0);
        }
        let snapshots = self
            .journal
            .active_pool_snapshots()
            .map_err(|_| IngressError::Journal)?;
        let mut drained = 0;
        let mut candidates = Vec::new();
        for snapshot in snapshots {
            let document: SnapshotDocument =
                serde_json::from_str(&snapshot.config_json).map_err(|_| IngressError::Journal)?;
            let terminal = self
                .journal
                .run_is_terminal(
                    document.base_repository_id,
                    document.run_id,
                    document.run_attempt,
                )
                .map_err(|_| IngressError::Journal)?;
            let expired = snapshot.expires_at <= now();
            if terminal || expired || snapshot.state == "draining" {
                candidates.push((snapshot, document));
                if candidates.len() == limit {
                    break;
                }
            }
        }
        for (snapshot, document) in candidates {
            let Some(ingress_settings) = document.central.ingress.as_ref() else {
                return Err(IngressError::Configuration);
            };
            self.garm
                .verify_origin(&ingress_settings.garm_api_origin)
                .map_err(|_| IngressError::Configuration)?;

            let repository = ingress_settings
                .repositories
                .get(&snapshot.repository_id.to_string())
                .ok_or(IngressError::Configuration)?;
            self.journal
                .set_pool_snapshot_state(&snapshot.id, "draining", snapshot.garm_pool_id.as_deref())
                .map_err(|_| IngressError::Journal)?;
            let deadline = tokio::time::Instant::now() + self.operation_budget();
            let reconciled =
                tokio::time::timeout_at(deadline, self.garm.disable_request_pool(&snapshot)).await;
            let pool_id = match reconciled {
                Ok(Ok(Some(pool_id))) => pool_id,
                Ok(Err(PoolError::Configuration)) => return Err(IngressError::Configuration),
                Ok(Err(PoolError::NotFound)) => {
                    if let Some(pool_id) = snapshot.garm_pool_id.as_deref()
                        && self
                            .journal
                            .pool_outstanding_operations(pool_id)
                            .map_err(|_| IngressError::Journal)?
                            == 0
                    {
                        self.journal
                            .set_pool_snapshot_state(&snapshot.id, "deleted", Some(pool_id))
                            .map_err(|_| IngressError::Journal)?;
                        drained += 1;
                    }
                    continue;
                }
                Ok(Ok(None)) | Ok(Err(_)) | Err(_) => continue,
            };
            self.journal
                .set_pool_snapshot_state(&snapshot.id, "draining", Some(&pool_id))
                .map_err(|_| IngressError::Journal)?;
            let outstanding = self
                .journal
                .pool_outstanding_operations(&pool_id)
                .map_err(|_| IngressError::Journal)?;
            match tokio::time::timeout_at(deadline, async {
                if outstanding == 0 {
                    self.garm
                        .delete_failed_unspawned_instances(
                            &pool_id,
                            &snapshot.id,
                            ingress_settings,
                            &self.journal,
                        )
                        .await?;
                }

                self.garm
                    .delete_drained_pool(&repository.garm_repository_id, &pool_id, outstanding)
                    .await
            })
            .await
            {
                Ok(Ok(())) => {
                    self.journal
                        .set_pool_snapshot_state(&snapshot.id, "deleted", Some(&pool_id))
                        .map_err(|_| IngressError::Journal)?;
                    drained += 1;
                }
                Ok(Err(PoolError::NotFound)) if outstanding == 0 => {
                    self.journal
                        .set_pool_snapshot_state(&snapshot.id, "deleted", Some(&pool_id))
                        .map_err(|_| IngressError::Journal)?;
                    drained += 1;
                }
                _ => {}
            }
        }
        Ok(drained)
    }

    pub(super) async fn disable_admission_pool(
        &self,
        repository_id: u64,
        token: &str,
    ) -> Result<(), bool> {
        let Some(snapshot) = self
            .journal
            .pool_snapshot(repository_id, token)
            .map_err(|_| true)?
        else {
            return Ok(());
        };
        if matches!(snapshot.state.as_str(), "deleted" | "rejected") {
            return Ok(());
        }
        if let Some(pool_id) = self
            .garm
            .disable_request_pool(&snapshot)
            .await
            .map_err(|_| true)?
        {
            self.journal
                .set_pool_snapshot_state(&snapshot.id, "draining", Some(&pool_id))
                .map_err(|_| true)?;
        }
        Ok(())
    }
}
