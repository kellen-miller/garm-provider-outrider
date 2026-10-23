use super::{
    BTreeMap, CandidateHistory, CreateHistoryRecord, InterruptionRecord, Journal, JournalError,
    RETAINED_CONFIG_ALLOCATION_PREDICATE, RunningHistoryRecord, UnknownCreateAttempt,
    same_interruption_provenance, same_interruption_source_record,
};
use rusqlite::{OptionalExtension, params};

impl Journal {
    /// Persist one provider-qualified interruption event once per native incarnation.
    ///
    /// # Errors
    /// Returns `JournalError` for inconsistent allocation provenance or storage failure.
    pub fn record_interruption(
        &self,
        record: &InterruptionRecord,
        identity: &crate::cloud::PlacementIdentity,
    ) -> Result<bool, JournalError> {
        Ok(self.record_interruptions(&[(record, identity)])? > 0)
    }

    /// Validate and persist one observation batch atomically before cleanup can be scheduled.
    ///
    /// # Errors
    /// Returns `JournalError` for conflicting source revisions or storage failure.
    pub fn record_interruptions(
        &self,
        records: &[(&InterruptionRecord, &crate::cloud::PlacementIdentity)],
    ) -> Result<usize, JournalError> {
        if records.is_empty() {
            return Ok(0);
        }
        let mut connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let transaction = connection.transaction()?;
        let mut batch = BTreeMap::new();
        for (record, identity) in records {
            let identity_json =
                serde_json::to_string(identity).map_err(|_| rusqlite::Error::InvalidQuery)?;
            if record.reason_fields_json.len() > 4096
                || record.provider_pin_json.len() > 4096
                || record.source_id.is_empty()
                || record.source_id.len() > 512
                || record.event_id.is_empty()
                || record.event_id.len() > 1024
                || chrono::DateTime::parse_from_rfc3339(&record.occurred_at).is_err()
                || chrono::DateTime::parse_from_rfc3339(&record.observed_at).is_err()
            {
                return Err(rusqlite::Error::InvalidQuery.into());
            }
            let stored = transaction.query_row(
                "SELECT controller_id, pool_id, catalog_fingerprint, native_identity_json FROM allocations WHERE allocation_id=?1",
                [&record.allocation_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?, row.get::<_, Option<String>>(3)?)),
            )?;
            if stored.0 != record.controller_id
                || stored.1 != record.pool_id
                || stored.2.as_deref() != Some(record.catalog_fingerprint.as_str())
                || stored.3.as_deref() != Some(identity_json.as_str())
            {
                return Err(rusqlite::Error::InvalidQuery.into());
            }
            let key = (
                identity.cloud.clone(),
                identity.account_id.clone(),
                identity.location.clone(),
                identity.resource_id.clone(),
                record.event_id.clone(),
            );
            if let Some(previous) = batch.insert(key.clone(), record)
                && !same_interruption_source_record(previous, record)
            {
                return Err(rusqlite::Error::InvalidQuery.into());
            }
            let existing = transaction
                .query_row(
                    "SELECT allocation_id, controller_id, pool_id, catalog_fingerprint, configuration_fingerprint, provider_pin_json, identity_json, source_api, api_version, source_id, event_id, occurred_at, observed_at, reason_fields_json FROM interruption_events WHERE cloud=?1 AND account_id=?2 AND location=?3 AND resource_id=?4 AND event_id=?5",
                    rusqlite::params![identity.cloud, identity.account_id, identity.location, identity.resource_id, record.event_id],
                    |row| {
                        Ok(InterruptionRecord {
                            allocation_id: row.get(0)?,
                            controller_id: row.get(1)?,
                            pool_id: row.get(2)?,
                            catalog_fingerprint: row.get(3)?,
                            configuration_fingerprint: row.get(4)?,
                            provider_pin_json: row.get(5)?,
                            identity_json: row.get(6)?,
                            source_api: row.get(7)?,
                            api_version: row.get(8)?,
                            source_id: row.get(9)?,
                            event_id: row.get(10)?,
                            occurred_at: row.get(11)?,
                            observed_at: row.get(12)?,
                            reason_fields_json: row.get(13)?,
                        })
                    },
                )
                .optional()?;
            if existing.is_some_and(|existing| {
                !same_interruption_provenance(&existing, record, &identity_json)
            }) {
                return Err(rusqlite::Error::InvalidQuery.into());
            }
        }
        let mut inserted = 0;
        for (record, identity) in records {
            let identity_json =
                serde_json::to_string(identity).map_err(|_| rusqlite::Error::InvalidQuery)?;
            inserted += transaction.execute(
                "INSERT OR IGNORE INTO interruption_events(allocation_id, controller_id, pool_id, catalog_fingerprint, configuration_fingerprint, provider_pin_json, identity_json, cloud, account_id, location, resource_id, source_api, api_version, source_id, event_id, occurred_at, observed_at, reason_fields_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                rusqlite::params![record.allocation_id, record.controller_id, record.pool_id, record.catalog_fingerprint, record.configuration_fingerprint, record.provider_pin_json, identity_json, identity.cloud, identity.account_id, identity.location, identity.resource_id, record.source_api, record.api_version, record.source_id, record.event_id, record.occurred_at, record.observed_at, record.reason_fields_json],
            )?;
        }
        transaction.commit()?;
        Ok(inserted)
    }

    /// Read durable interruption evidence for one logical allocation.
    ///
    /// # Errors
    /// Returns `JournalError` when storage access fails.
    pub fn interruption_events(
        &self,
        allocation_id: &str,
    ) -> Result<Vec<InterruptionRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT allocation_id, controller_id, pool_id, catalog_fingerprint, configuration_fingerprint, provider_pin_json, identity_json, source_api, api_version, source_id, event_id, occurred_at, observed_at, reason_fields_json FROM interruption_events WHERE allocation_id=?1 ORDER BY occurred_at, event_id",
        )?;
        let rows = statement.query_map([allocation_id], |row| {
            Ok(InterruptionRecord {
                allocation_id: row.get(0)?,
                controller_id: row.get(1)?,
                pool_id: row.get(2)?,
                catalog_fingerprint: row.get(3)?,
                configuration_fingerprint: row.get(4)?,
                provider_pin_json: row.get(5)?,
                identity_json: row.get(6)?,
                source_api: row.get(7)?,
                api_version: row.get(8)?,
                source_id: row.get(9)?,
                event_id: row.get(10)?,
                occurred_at: row.get(11)?,
                observed_at: row.get(12)?,
                reason_fields_json: row.get(13)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Persist a definitive positive child Create result and its measured latency.
    ///
    /// # Errors
    /// Returns `JournalError` when the dispatched attempt or history write fails.
    pub fn finish_child_create(
        &self,
        attempt_number: u32,
        record: &CreateHistoryRecord,
    ) -> Result<(), JournalError> {
        let mut connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let transaction = connection.transaction()?;
        let updated = transaction.execute(
            "UPDATE child_attempts SET state='created', detail='positive child return', completed_at=?3 WHERE allocation_id=?1 AND attempt_number=?2 AND state='dispatched'",
            rusqlite::params![record.allocation_id, i64::from(attempt_number), Self::now() as i64],
        )?;
        if updated != 1 {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        transaction.execute(
            "INSERT OR IGNORE INTO create_history(allocation_id, candidate_key, catalog_fingerprint, provider_pin_json, observed_at_millis, latency_nanos) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![record.allocation_id, record.candidate_key, record.catalog_fingerprint, record.provider_pin_json, i64::try_from(record.observed_at_millis).map_err(|_| rusqlite::Error::InvalidQuery)?, record.latency_nanos.map(i64::try_from).transpose().map_err(|_| rusqlite::Error::InvalidQuery)?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Persist an actual native running-state sample or a continuity barrier.
    ///
    /// # Errors
    /// Returns `JournalError` when the sample cannot be stored or validated.
    pub fn record_running_history(
        &self,
        record: &RunningHistoryRecord,
        minimum_interval_millis: u64,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        if record.running {
            let previous = connection.query_row(
                "SELECT MAX(observed_at_millis) FROM running_history WHERE allocation_id=?1 AND session_id=?2",
                rusqlite::params![record.allocation_id, record.session_id],
                |row| row.get::<_, Option<i64>>(0),
            )?;
            if previous.is_some_and(|previous| {
                u128::try_from(previous).unwrap_or_default() + u128::from(minimum_interval_millis)
                    > record.observed_at_millis
            }) {
                return Ok(());
            }
        }
        connection.execute(
            "INSERT INTO running_history(allocation_id, candidate_key, catalog_fingerprint, provider_pin_json, identity_json, session_id, observed_at_millis, running) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(allocation_id, session_id, observed_at_millis) DO UPDATE SET candidate_key=excluded.candidate_key, catalog_fingerprint=excluded.catalog_fingerprint, provider_pin_json=excluded.provider_pin_json, identity_json=excluded.identity_json, running=0 WHERE excluded.running=0 OR running_history.running=0",
            rusqlite::params![record.allocation_id, record.candidate_key, record.catalog_fingerprint, record.provider_pin_json, record.identity_json, record.session_id, i64::try_from(record.observed_at_millis).map_err(|_| rusqlite::Error::InvalidQuery)?, record.running],
        )?;
        Ok(())
    }

    /// Persist one verified physical interruption for the allocation incarnation.
    ///
    /// # Errors
    /// Returns `JournalError` when the event cannot be stored or validated.
    pub fn record_interruption_history(
        &self,
        allocation_id: &str,
        candidate_key: &str,
        catalog_fingerprint: &str,
        provider_pin_json: &str,
        occurred_at_millis: u128,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute(
            "INSERT OR IGNORE INTO interruption_history(allocation_id, candidate_key, catalog_fingerprint, provider_pin_json, occurred_at_millis) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![allocation_id, candidate_key, catalog_fingerprint, provider_pin_json, i64::try_from(occurred_at_millis).map_err(|_| rusqlite::Error::InvalidQuery)?],
        )?;
        Ok(())
    }

    /// Compact bounded terminal payload and redundant history batches.
    ///
    /// # Errors
    /// Returns `JournalError` when the maintenance transaction fails.
    pub fn maintain_retention(
        &self,
        configured_history_window_seconds: u64,
        limit: usize,
    ) -> Result<usize, JournalError> {
        if limit == 0 {
            return Ok(0);
        }

        let mut connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let transaction = connection.transaction()?;
        let now = Self::now() as i64;
        let terminal_cutoff = now.saturating_sub(30 * 24 * 60 * 60);
        let retention_seconds = configured_history_window_seconds.max(90 * 24 * 60 * 60);
        let history_cutoff_millis = now.saturating_mul(1000).saturating_sub(
            i64::try_from(retention_seconds)
                .unwrap_or(i64::MAX)
                .saturating_mul(1000),
        );
        let batch = i64::try_from(limit.min(32)).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut changed = 0usize;

        changed += transaction.execute(
            "UPDATE webhook_deliveries SET headers_json='', body=x'' WHERE delivery_id IN (SELECT delivery_id FROM webhook_deliveries WHERE terminal_at IS NOT NULL AND terminal_at<=?1 AND (length(headers_json)>0 OR length(body)>0) ORDER BY terminal_at, delivery_id LIMIT ?2)",
            params![terminal_cutoff, batch],
        )?;
        changed += transaction.execute(
            "UPDATE ingress_outbox SET headers_json='', body=x'' WHERE id IN (SELECT id FROM ingress_outbox WHERE terminal_at IS NOT NULL AND terminal_at<=?1 AND (length(headers_json)>0 OR length(body)>0) ORDER BY terminal_at, id LIMIT ?2)",
            params![terminal_cutoff, batch],
        )?;
        let compact_snapshots = format!(
            "UPDATE pool_snapshots SET config_json='{{\"_outrider_snapshot\":\"compacted\"}}', compacted_at=?1 WHERE id IN (SELECT snapshot.id FROM pool_snapshots snapshot WHERE snapshot.state IN ('deleted','rejected') AND snapshot.terminal_at IS NOT NULL AND snapshot.terminal_at<=?2 AND snapshot.compacted_at IS NULL AND NOT EXISTS (SELECT 1 FROM allocations WHERE allocations.snapshot_id=snapshot.id AND ({RETAINED_CONFIG_ALLOCATION_PREDICATE})) ORDER BY snapshot.terminal_at, snapshot.id LIMIT ?3)"
        );
        changed += transaction.execute(&compact_snapshots, params![now, terminal_cutoff, batch])?;

        let mut statement = transaction.prepare(
            "SELECT allocation_id, candidate_key, catalog_fingerprint, provider_pin_json, observed_at_millis FROM create_history WHERE latency_nanos IS NOT NULL AND observed_at_millis<?1 ORDER BY observed_at_millis, allocation_id LIMIT ?2",
        )?;
        let rows = statement.query_map(params![history_cutoff_millis, batch], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let old_latency = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for (allocation_id, candidate_key, fingerprint, pin, observed_at) in &old_latency {
            transaction.execute(
                "INSERT INTO history_retention_floors(candidate_key, catalog_fingerprint, provider_pin_json, dimension, floor_millis) VALUES (?1, ?2, ?3, 'latency', ?4) ON CONFLICT(candidate_key, catalog_fingerprint, provider_pin_json, dimension) DO UPDATE SET floor_millis=MAX(floor_millis, excluded.floor_millis)",
                params![candidate_key, fingerprint, pin, observed_at],
            )?;
            changed += transaction.execute(
                "UPDATE create_history SET latency_nanos=NULL WHERE allocation_id=?1 AND observed_at_millis=?2",
                params![allocation_id, observed_at],
            )?;
        }

        let mut statement = transaction.prepare(
            "SELECT allocation_id, candidate_key, catalog_fingerprint, provider_pin_json, session_id, observed_at_millis FROM running_history WHERE observed_at_millis<?1 ORDER BY observed_at_millis, allocation_id, session_id LIMIT ?2",
        )?;
        let rows = statement.query_map(params![history_cutoff_millis, batch], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        let old_samples = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for (allocation_id, candidate_key, fingerprint, pin, session_id, observed_at) in
            &old_samples
        {
            transaction.execute(
                "INSERT INTO history_retention_floors(candidate_key, catalog_fingerprint, provider_pin_json, dimension, floor_millis) VALUES (?1, ?2, ?3, 'running', ?4) ON CONFLICT(candidate_key, catalog_fingerprint, provider_pin_json, dimension) DO UPDATE SET floor_millis=MAX(floor_millis, excluded.floor_millis)",
                params![candidate_key, fingerprint, pin, observed_at],
            )?;
            changed += transaction.execute(
                "DELETE FROM running_history WHERE allocation_id=?1 AND session_id=?2 AND observed_at_millis=?3",
                params![allocation_id, session_id, observed_at],
            )?;
        }

        transaction.commit()?;
        Ok(changed)
    }

    /// Load scoped metrics and verified exposure intervals for policy ranking.
    ///
    /// # Errors
    /// Returns `JournalError` when the scoped query or stored data is invalid.
    pub fn candidate_history(
        &self,
        candidate_key: &str,
        catalog_fingerprint: &str,
        provider_pin_json: &str,
        window_start_millis: u128,
        window_end_millis: u128,
        maximum_gap_millis: u64,
    ) -> Result<CandidateHistory, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let window_start =
            i64::try_from(window_start_millis).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let window_end =
            i64::try_from(window_end_millis).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let latency_coverage_complete = !connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM history_retention_floors WHERE candidate_key=?1 AND catalog_fingerprint=?2 AND provider_pin_json=?3 AND dimension='latency' AND ?4<=floor_millis)",
            rusqlite::params![candidate_key, catalog_fingerprint, provider_pin_json, window_start],
            |row| row.get::<_, bool>(0),
        )?;
        let running_coverage_complete = !connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM history_retention_floors WHERE candidate_key=?1 AND catalog_fingerprint=?2 AND provider_pin_json=?3 AND dimension='running' AND ?4<=floor_millis)",
            rusqlite::params![candidate_key, catalog_fingerprint, provider_pin_json, window_start],
            |row| row.get::<_, bool>(0),
        )?;
        let mut latencies = connection.prepare(
            "SELECT latency_nanos FROM create_history WHERE candidate_key=?1 AND catalog_fingerprint=?2 AND provider_pin_json=?3 AND observed_at_millis>=?4 AND observed_at_millis<=?5 AND latency_nanos IS NOT NULL ORDER BY observed_at_millis",
        )?;
        let create_latencies_nanos = latencies
            .query_map(
                rusqlite::params![
                    candidate_key,
                    catalog_fingerprint,
                    provider_pin_json,
                    window_start,
                    window_end
                ],
                |row| {
                    u64::try_from(row.get::<_, i64>(0)?).map_err(|_| rusqlite::Error::InvalidQuery)
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let latest_create = connection.query_row(
            "SELECT MAX(create_history.observed_at_millis) FROM create_history
             JOIN allocations ON allocations.allocation_id=create_history.allocation_id
             WHERE create_history.candidate_key=?1 AND create_history.catalog_fingerprint=?2 AND create_history.provider_pin_json=?3
               AND create_history.observed_at_millis>=?4 AND create_history.observed_at_millis<=?5
               AND allocations.native_identity_json IS NOT NULL",
            rusqlite::params![candidate_key, catalog_fingerprint, provider_pin_json, window_start, window_end],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        let latest_latency = connection.query_row(
            "SELECT MAX(observed_at_millis) FROM create_history WHERE candidate_key=?1 AND catalog_fingerprint=?2 AND provider_pin_json=?3 AND observed_at_millis>=?4 AND observed_at_millis<=?5 AND latency_nanos IS NOT NULL",
            rusqlite::params![candidate_key, catalog_fingerprint, provider_pin_json, window_start, window_end],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        let definitive_create_outcomes = connection.query_row(
            "SELECT COUNT(*) FROM create_history
             JOIN allocations ON allocations.allocation_id=create_history.allocation_id
             WHERE create_history.candidate_key=?1 AND create_history.catalog_fingerprint=?2 AND create_history.provider_pin_json=?3
               AND create_history.observed_at_millis>=?4 AND create_history.observed_at_millis<=?5
               AND allocations.native_identity_json IS NOT NULL",
            rusqlite::params![candidate_key, catalog_fingerprint, provider_pin_json, window_start, window_end],
            |row| u64::try_from(row.get::<_, i64>(0)?).map_err(|_| rusqlite::Error::InvalidQuery),
        )?;
        let measured_latency_samples = if latency_coverage_complete {
            u64::try_from(create_latencies_nanos.len())
                .map_err(|_| rusqlite::Error::InvalidQuery)?
        } else {
            0
        };
        let latest_interruption = connection.query_row(
            "SELECT MAX(occurred_at_millis) FROM interruption_history WHERE candidate_key=?1 AND catalog_fingerprint=?2 AND provider_pin_json=?3 AND occurred_at_millis>=?4 AND occurred_at_millis<=?5",
            rusqlite::params![candidate_key, catalog_fingerprint, provider_pin_json, window_start, window_end],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        let interruptions: u64 = connection.query_row(
            "SELECT COUNT(*) FROM interruption_history WHERE candidate_key=?1 AND catalog_fingerprint=?2 AND provider_pin_json=?3 AND occurred_at_millis>=?4 AND occurred_at_millis<=?5",
            rusqlite::params![candidate_key, catalog_fingerprint, provider_pin_json, window_start, window_end],
            |row| u64::try_from(row.get::<_, i64>(0)?).map_err(|_| rusqlite::Error::InvalidQuery),
        )?;
        let mut samples = connection.prepare(
            "SELECT allocation_id, session_id, observed_at_millis, running FROM running_history WHERE candidate_key=?1 AND catalog_fingerprint=?2 AND provider_pin_json=?3 AND observed_at_millis>=?4 AND observed_at_millis<=?5 ORDER BY allocation_id, session_id, observed_at_millis",
        )?;
        let rows = samples.query_map(
            rusqlite::params![
                candidate_key,
                catalog_fingerprint,
                provider_pin_json,
                window_start,
                window_end
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    u64::try_from(row.get::<_, i64>(2)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    row.get::<_, bool>(3)?,
                ))
            },
        )?;
        let mut previous: Option<(String, String, u64, bool)> = None;
        let mut latest_observation_millis: Option<u128> = None;
        let mut observation_samples = 0;
        let mut running_exposure_millis = 0u128;
        for row in rows {
            let sample = row?;
            if let Some((allocation_id, session, previous_at, previous_running)) = previous
                && allocation_id == sample.0
                && session == sample.1
                && previous_running
                && sample.3
                && sample.2 > previous_at
                && sample.2 - previous_at <= maximum_gap_millis
            {
                running_exposure_millis += u128::from(sample.2 - previous_at);
                observation_samples += 1;
                latest_observation_millis = Some(
                    latest_observation_millis
                        .unwrap_or_default()
                        .max(u128::from(sample.2)),
                );
            }
            previous = Some(sample);
        }
        Ok(CandidateHistory {
            create_latencies_nanos: if latency_coverage_complete {
                create_latencies_nanos
            } else {
                Vec::new()
            },
            definitive_create_outcomes,
            measured_latency_samples,
            interruptions,
            running_exposure_millis: if running_coverage_complete {
                running_exposure_millis
            } else {
                0
            },
            latest_observation_millis: running_coverage_complete
                .then_some(latest_observation_millis)
                .flatten(),
            latest_create_millis: latest_create
                .map(|value| u128::try_from(value).unwrap_or_default()),
            latest_latency_millis: latency_coverage_complete
                .then_some(latest_latency)
                .flatten()
                .map(|value| u128::try_from(value).unwrap_or_default()),
            latest_interruption_millis: latest_interruption
                .map(|value| u128::try_from(value).unwrap_or_default()),
            observation_samples,
            latency_coverage_complete,
            running_coverage_complete,
        })
    }

    /// Read dispatched creates without a verified allocation outcome.
    ///
    /// # Errors
    /// Returns `JournalError` when the scoped query or stored data is invalid.
    pub fn unknown_create_attempts(
        &self,
        window_start_millis: u128,
        window_end_millis: u128,
    ) -> Result<Vec<UnknownCreateAttempt>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let window_start =
            i64::try_from(window_start_millis).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let window_end =
            i64::try_from(window_end_millis).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut statement = connection.prepare(
            "SELECT child_attempts.offer_json, child_attempts.binding_json, allocations.catalog_fingerprint,
                    COALESCE(child_attempts.dispatched_at, allocations.dispatched_at) * 1000
             FROM child_attempts
             JOIN allocations ON allocations.allocation_id=child_attempts.allocation_id
             WHERE child_attempts.state IN ('dispatched', 'ambiguous')
               AND NOT EXISTS (SELECT 1 FROM create_history WHERE create_history.allocation_id=child_attempts.allocation_id)
               AND (COALESCE(child_attempts.dispatched_at, allocations.dispatched_at) IS NULL
                    OR COALESCE(child_attempts.dispatched_at, allocations.dispatched_at) BETWEEN (?1 / 1000) AND (?2 / 1000))
             UNION ALL
             SELECT child_attempts.offer_json, child_attempts.binding_json, allocations.catalog_fingerprint,
                    COALESCE((SELECT MAX(observed_at_millis) FROM create_history WHERE create_history.allocation_id=child_attempts.allocation_id),
                             COALESCE(child_attempts.dispatched_at, allocations.dispatched_at) * 1000)
             FROM child_attempts
             JOIN allocations ON allocations.allocation_id=child_attempts.allocation_id
             WHERE child_attempts.state IN ('created', 'ambiguous')
               AND ((child_attempts.state='created'
                     AND NOT EXISTS (SELECT 1 FROM create_history WHERE create_history.allocation_id=child_attempts.allocation_id)
                     AND (COALESCE(child_attempts.dispatched_at, allocations.dispatched_at) IS NULL
                          OR COALESCE(child_attempts.dispatched_at, allocations.dispatched_at) BETWEEN (?1 / 1000) AND (?2 / 1000)))
                    OR (allocations.native_identity_json IS NULL
                        AND EXISTS (SELECT 1 FROM create_history WHERE create_history.allocation_id=child_attempts.allocation_id
                                    AND observed_at_millis BETWEEN ?1 AND ?2)))",
        )?;
        let rows = statement.query_map(rusqlite::params![window_start, window_end], |row| {
            Ok(UnknownCreateAttempt {
                offer_json: row.get(0)?,
                binding_json: row.get(1)?,
                catalog_fingerprint: row.get(2)?,
                observed_at_millis: row
                    .get::<_, Option<i64>>(3)?
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
