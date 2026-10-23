use super::history::candidate_history_key;
use super::native::{allocation_dispatch, azure_history_has_unique_incarnation};
use super::{
    AllocationError, AllocationRecord, AllocationService, CatalogOffer, Duration, Instant,
    InterruptionRecord, Ordering, PlacementIdentity, ProviderBinding, RunningHistoryRecord,
    SystemTime, unix_now_millis,
};
use crate::providers::provider_pin_json;

impl AllocationService {
    /// Poll a bounded rotating batch of owned Spot allocations before cleanup.
    ///
    /// # Errors
    /// Returns `AllocationError::Ambiguous` when a provider read or durable event write is unresolved.
    pub async fn observe_interruptions(&self, limit: usize) -> Result<usize, AllocationError> {
        use std::{future::Future, task::Poll};

        if limit == 0 {
            return Ok(0);
        }
        let allocations = self
            .journal
            .observable_allocations()
            .map_err(|_| AllocationError::Journal)?;
        if allocations.is_empty() {
            self.interruption_recovery_offset
                .store(0, Ordering::Relaxed);
            return Ok(0);
        }
        let history = self
            .journal
            .all_allocations()
            .map_err(|_| AllocationError::Journal)?;
        let count = limit.min(allocations.len());
        let start = self
            .interruption_recovery_offset
            .fetch_add(count, Ordering::Relaxed)
            % allocations.len();
        let deadline = Instant::now() + Duration::from_secs(self.provider_timeout_seconds()?);
        let mut pending = (0..count)
            .map(|index| {
                let allocation = &allocations[(start + index) % allocations.len()];
                Some(Box::pin(self.observe_allocation_interruption(
                    allocation, &history, deadline,
                )))
            })
            .collect::<Vec<_>>();
        let mut observed = 0;
        let mut failed = false;
        std::future::poll_fn(|context| {
            let mut active = false;
            for work in &mut pending {
                let Some(future) = work else {
                    continue;
                };
                if let Poll::Ready(result) = future.as_mut().poll(context) {
                    match result {
                        Ok(count) => observed += count,
                        Err(_) => failed = true,
                    }
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
            Ok(observed)
        }
    }

    pub(super) async fn observe_allocation_interruption(
        &self,
        allocation: &AllocationRecord,
        history: &[AllocationRecord],
        deadline: Instant,
    ) -> Result<usize, AllocationError> {
        if !self
            .journal
            .interruption_events(&allocation.allocation_id)
            .map_err(|_| AllocationError::Journal)?
            .is_empty()
        {
            self.record_interruption_history(allocation)?;
            self.journal
                .schedule_allocation_cleanup(
                    &allocation.allocation_id,
                    "retained verified provider interruption",
                )
                .map_err(|_| AllocationError::Journal)?;
            return Ok(1);
        }
        let Ok((offer, binding, Some(identity))) = allocation_dispatch(allocation) else {
            return Err(AllocationError::Ambiguous);
        };
        if offer.candidate.purchasing != "spot" {
            if self.config.history.is_some() {
                let observed = self
                    .inspect_allocation_identity(allocation, &identity, deadline)
                    .await;
                let running = observed.as_ref().is_ok_and(|instance| {
                    instance
                        .as_ref()
                        .is_some_and(|instance| instance.state == "running")
                });
                self.record_running_observation(allocation, &offer, &binding, &identity, running)?;
                if observed.is_err() {
                    return Err(AllocationError::Ambiguous);
                }
            }
            return Ok(0);
        }
        let Some(catalog) = self.catalogs.iter().find(|catalog| {
            Some(catalog.configuration_fingerprint.as_str())
                == allocation.catalog_fingerprint.as_deref()
                && catalog.binding_id == binding.id
                && catalog.supports_identity(&identity)
        }) else {
            return Err(AllocationError::Ambiguous);
        };
        if identity.cloud.eq_ignore_ascii_case("azure")
            && !azure_history_has_unique_incarnation(history, allocation, &identity)
        {
            self.record_running_observation(allocation, &offer, &binding, &identity, false)?;
            eprintln!(
                "outrider interruption observation unknown for ambiguous Azure resource history"
            );
            return Ok(0);
        }
        let evidence = match catalog.observe_interruption(&identity, deadline).await {
            Ok(evidence) => evidence,
            Err(_) => {
                self.record_running_observation(allocation, &offer, &binding, &identity, false)?;
                eprintln!(
                    "outrider interruption observation unknown for {} allocation {}",
                    identity.cloud, allocation.allocation_id
                );
                return Err(AllocationError::Ambiguous);
            }
        };
        let observed_at = chrono::DateTime::<chrono::Utc>::from(SystemTime::now());
        let allocation_started = if identity.cloud.eq_ignore_ascii_case("azure") {
            allocation.returned_at
        } else {
            allocation.dispatched_at
        }
        .ok_or(AllocationError::Ambiguous)?;
        let earliest_delete = self
            .journal
            .first_allocation_delete_intent(&allocation.allocation_id)
            .map_err(|_| AllocationError::Journal)?;
        let provider_pin_json = serde_json::json!({
            "binding_id":binding.id,
            "source_revision":binding.source_revision,
            "binary_sha256":binding.executable_sha256,
            "configuration_sha256":binding.configuration_sha256,
        })
        .to_string();
        let mut records = Vec::new();
        for evidence in evidence {
            let Ok(occurred) = chrono::DateTime::parse_from_rfc3339(&evidence.occurred_at) else {
                return Err(AllocationError::Ambiguous);
            };
            if occurred.timestamp() <= i64::try_from(allocation_started).unwrap_or(i64::MAX)
                || occurred.with_timezone(&chrono::Utc) > observed_at
                || earliest_delete.is_some_and(|started| {
                    i64::try_from(started).is_ok_and(|started| started <= occurred.timestamp())
                })
            {
                continue;
            }
            let record = InterruptionRecord {
                allocation_id: allocation.allocation_id.clone(),
                controller_id: allocation.controller_id.clone(),
                pool_id: allocation.pool_id.clone(),
                catalog_fingerprint: allocation
                    .catalog_fingerprint
                    .clone()
                    .ok_or(AllocationError::Journal)?,
                configuration_fingerprint: binding.configuration_sha256.clone(),
                provider_pin_json: provider_pin_json.clone(),
                identity_json: serde_json::to_string(&identity)
                    .map_err(|_| AllocationError::Journal)?,
                source_api: evidence.source_api,
                api_version: evidence.api_version,
                source_id: evidence.source_id,
                event_id: evidence.event_id,
                occurred_at: evidence.occurred_at,
                observed_at: observed_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                reason_fields_json: evidence.reason_fields_json,
            };
            records.push(record);
        }
        if !records.is_empty() {
            let batch = records
                .iter()
                .map(|record| (record, &identity))
                .collect::<Vec<_>>();
            if self.journal.record_interruptions(&batch).is_err() {
                self.record_running_observation(allocation, &offer, &binding, &identity, false)?;
                return Err(AllocationError::Ambiguous);
            }

            self.record_interruption_history(allocation)?;
            self.journal
                .schedule_allocation_cleanup(
                    &allocation.allocation_id,
                    "verified provider interruption",
                )
                .map_err(|_| AllocationError::Journal)?;
        } else if self.config.history.is_some() {
            let observed = self
                .inspect_allocation_identity(allocation, &identity, deadline)
                .await;
            let running = observed.as_ref().is_ok_and(|instance| {
                instance
                    .as_ref()
                    .is_some_and(|instance| instance.state == "running")
            });
            self.record_running_observation(allocation, &offer, &binding, &identity, running)?;
            if observed.is_err() {
                return Err(AllocationError::Ambiguous);
            }
        }
        Ok(records.len())
    }

    pub(super) fn record_running_observation(
        &self,
        allocation: &AllocationRecord,
        offer: &CatalogOffer,
        binding: &ProviderBinding,
        identity: &PlacementIdentity,
        running: bool,
    ) -> Result<(), AllocationError> {
        let Some(settings) = &self.config.history else {
            return Ok(());
        };
        let catalog_fingerprint = allocation
            .catalog_fingerprint
            .as_deref()
            .ok_or(AllocationError::Journal)?;
        self.journal
            .record_running_history(
                &RunningHistoryRecord {
                    allocation_id: allocation.allocation_id.clone(),
                    candidate_key: candidate_history_key(offer, catalog_fingerprint, binding)?,
                    catalog_fingerprint: catalog_fingerprint.to_owned(),
                    provider_pin_json: provider_pin_json(binding),
                    identity_json: serde_json::to_string(identity)
                        .map_err(|_| AllocationError::Journal)?,
                    session_id: self.history_session_id.clone(),
                    observed_at_millis: unix_now_millis(),
                    running,
                },
                settings.sampling_interval_seconds.saturating_mul(1000),
            )
            .map_err(|_| AllocationError::Journal)
    }

    pub(super) fn record_interruption_history(
        &self,
        allocation: &AllocationRecord,
    ) -> Result<(), AllocationError> {
        if self.config.history.is_none() {
            return Ok(());
        }
        let (offer, binding, _) = allocation_dispatch(allocation)?;
        let catalog_fingerprint = allocation
            .catalog_fingerprint
            .as_deref()
            .ok_or(AllocationError::Journal)?;
        let occurred_at = self
            .journal
            .interruption_events(&allocation.allocation_id)
            .map_err(|_| AllocationError::Journal)?
            .into_iter()
            .filter_map(|event| chrono::DateTime::parse_from_rfc3339(&event.occurred_at).ok())
            .map(|occurred| occurred.timestamp_millis())
            .min()
            .ok_or(AllocationError::Journal)?;
        self.journal
            .record_interruption_history(
                &allocation.allocation_id,
                &candidate_history_key(&offer, catalog_fingerprint, &binding)?,
                catalog_fingerprint,
                &provider_pin_json(&binding),
                u128::try_from(occurred_at).map_err(|_| AllocationError::Journal)?,
            )
            .map_err(|_| AllocationError::Journal)
    }
}
