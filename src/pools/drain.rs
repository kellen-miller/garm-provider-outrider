use super::{GarmClient, IngressSettings, Journal, PoolError, path_component};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize, PartialEq)]
struct FailedGarmInstance {
    id: String,
    name: String,
    pool_id: String,
    provider_name: String,
    status: String,
    runner_status: String,
    #[serde(default)]
    provider_id: String,
    #[serde(default)]
    agent_id: i64,
    #[serde(default)]
    addresses: Vec<Value>,
    job: Option<Value>,
}

impl GarmClient {
    /// Delete failed GARM records only for fenced, terminal allocations that never dispatched.
    ///
    /// # Errors
    /// Returns `PoolError` on invalid responses, journal failures or GARM errors.
    pub(crate) async fn delete_failed_unspawned_instances(
        &self,
        pool_id: &str,
        snapshot_id: &str,
        settings: &IngressSettings,
        journal: &Journal,
    ) -> Result<(), PoolError> {
        let path = format!("api/v1/pools/{}/instances", path_component(pool_id));
        let instances: Vec<FailedGarmInstance> =
            serde_json::from_value(self.request_json(reqwest::Method::GET, &path, None).await?)
                .map_err(|_| PoolError::Request)?;
        for instance in instances
            .into_iter()
            .take(settings.max_runners_per_pool as usize)
        {
            if instance.id.is_empty()
                || instance.name.is_empty()
                || instance.pool_id != pool_id
                || instance.provider_name != settings.provider_name
                || instance.status != "error"
                || instance.runner_status != "pending"
                || !instance.provider_id.is_empty()
                || instance.agent_id != 0
                || !instance.addresses.is_empty()
                || instance.job.is_some()
            {
                continue;
            }

            let Some(allocation) = journal
                .allocation(&settings.controller_id, &instance.name)
                .map_err(|_| PoolError::Request)?
            else {
                continue;
            };
            if allocation.runner_name != instance.name
                || allocation.pool_id != pool_id
                || allocation.snapshot_id != snapshot_id
                || allocation.state != "deleted"
                || !allocation.tombstone
                || allocation.dispatched_at.is_some()
                || allocation.returned_at.is_some()
                || allocation.provider_instance_json.is_some()
                || allocation.native_identity_json.is_some()
                || !journal
                    .child_attempts(&allocation.allocation_id)
                    .map_err(|_| PoolError::Request)?
                    .is_empty()
            {
                continue;
            }

            // The immutable database ID is accepted by the pinned v0.2.1 instance API,
            // as verified during live failed-creation cleanup.
            let path = format!("api/v1/instances/{}", path_component(&instance.id));
            let Some(current) = self
                .request_json_optional(reqwest::Method::GET, &path, None)
                .await?
            else {
                continue;
            };
            let current: FailedGarmInstance =
                serde_json::from_value(current).map_err(|_| PoolError::Request)?;
            if current != instance {
                continue;
            }

            self.request_json_optional(reqwest::Method::DELETE, &path, None)
                .await?;
        }

        Ok(())
    }

    /// Delete a pool only when GARM reports no instances and no operations remain.
    ///
    /// # Errors
    /// Returns `PoolError::Limit` while work remains, or `PoolError::Request` on GARM errors.
    pub async fn delete_drained_pool(
        &self,
        repository_id: &str,
        pool_id: &str,
        outstanding_operations: usize,
    ) -> Result<(), PoolError> {
        let detail_path = format!(
            "api/v1/repositories/{}/pools/{}",
            path_component(repository_id),
            path_component(pool_id)
        );
        let pool: Value = self
            .request_json_optional(reqwest::Method::GET, &detail_path, None)
            .await?
            .ok_or(PoolError::NotFound)?;
        if pool.get("enabled").and_then(Value::as_bool) == Some(true) {
            self.request_json(
                reqwest::Method::PUT,
                &detail_path,
                Some(&json!({"enabled": false})),
            )
            .await?;
        }
        if outstanding_operations != 0 {
            return Err(PoolError::Limit);
        }
        let instances_path = format!("api/v1/pools/{}/instances", path_component(pool_id));
        let instances: Value = self
            .request_json(reqwest::Method::GET, &instances_path, None)
            .await?;
        if !instances.as_array().is_some_and(Vec::is_empty) {
            return Err(PoolError::Limit);
        }
        self.request_json(reqwest::Method::DELETE, &detail_path, None)
            .await?;
        Ok(())
    }
}
