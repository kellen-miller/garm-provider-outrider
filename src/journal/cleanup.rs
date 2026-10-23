use super::{
    AllocationRecord, ChildAttemptRecord, CleanupGeneration, CleanupResolutionAudit,
    CleanupResolutionScope, Connection, Journal, JournalError, cleanup_backoff_seconds,
};
use rusqlite::{OptionalExtension, params};

impl Journal {
    /// Persist cleanup for an entire controller or pool before any child effects.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn request_allocation_cleanup(
        &self,
        controller_id: &str,
        pool_id: Option<&str>,
    ) -> Result<Vec<AllocationRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        let now = Self::now() as i64;
        tx.execute(
            "INSERT INTO allocation_actions(allocation_id, command, state, started_at, completed_at, detail, unresolved_create, reusable, next_attempt_at) SELECT allocation_id, 'DeleteInstance', CASE WHEN dispatched_at IS NULL THEN 'complete' ELSE 'pending' END, ?3, CASE WHEN dispatched_at IS NULL THEN ?3 ELSE NULL END, 'controller or pool cleanup requested', dispatched_at IS NOT NULL AND returned_at IS NULL, 1, ?3 FROM allocations WHERE controller_id=?1 AND (?2 IS NULL OR pool_id=?2) AND state!='deleted' AND NOT EXISTS(SELECT 1 FROM allocation_actions action WHERE action.allocation_id=allocations.allocation_id AND action.reusable=1)",
            params![controller_id, pool_id, now],
        )?;
        tx.execute(
            "UPDATE allocations SET tombstone=1, state=CASE WHEN dispatched_at IS NULL THEN 'deleted' ELSE 'deleting' END WHERE controller_id=?1 AND (?2 IS NULL OR pool_id=?2) AND state!='deleted'",
            params![controller_id, pool_id],
        )?;
        let allocations = {
            let mut statement = tx.prepare("SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE controller_id=?1 AND (?2 IS NULL OR pool_id=?2) ORDER BY created_at")?;
            statement
                .query_map(params![controller_id, pool_id], Self::read_allocation)?
                .collect::<Result<Vec<_>, _>>()?
        };
        tx.commit()?;
        Ok(allocations)
    }

    /// Close pre-spawn rows and orphan dispatched work after daemon startup.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn recover_startup_allocations(&self) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        let now = Self::now() as i64;
        tx.execute(
            "UPDATE allocation_actions SET state='ambiguous', completed_at=?1, detail=COALESCE(detail, 'action was interrupted by daemon shutdown') WHERE state='intent'",
            [now],
        )?;
        let retry_rows = {
            let mut statement = tx.prepare("SELECT action_id, attempt_count, last_attempt_at, next_attempt_at FROM allocation_actions WHERE reusable=1 AND command='DeleteInstance' AND state='ambiguous'")?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (action_id, attempts, last_attempt, next_attempt) in retry_rows {
            let due = match (next_attempt, last_attempt, attempts > 0) {
                (Some(due), _, _) => due,
                (None, Some(last_attempt), true) => {
                    last_attempt.saturating_add(cleanup_backoff_seconds(attempts))
                }
                (None, _, _) => now,
            };
            tx.execute("UPDATE allocation_actions SET state='pending', next_attempt_at=?2 WHERE action_id=?1 AND reusable=1", params![action_id, due])?;
        }
        tx.execute(
            "UPDATE allocations SET state='no_candidate', selected_offer_json=NULL, binding_json=NULL, account_id=NULL, catalog_fingerprint=NULL WHERE dispatched_at IS NULL AND tombstone=0 AND state IN ('reserved','selecting')",
            [],
        )?;
        tx.execute(
            "UPDATE allocations SET state='deleted' WHERE dispatched_at IS NULL AND tombstone=1 AND state IN ('reserved','selecting','deleting')",
            [],
        )?;
        tx.execute(
            "UPDATE allocations SET tombstone=1, state='deleting', detail=COALESCE(detail, 'dispatched create requires restart cleanup') WHERE dispatched_at IS NOT NULL AND state IN ('dispatched','ambiguous','deleting')",
            [],
        )?;
        tx.execute(
            "INSERT INTO allocation_actions(allocation_id, command, state, started_at, detail, unresolved_create, reusable, next_attempt_at) SELECT allocation_id, 'DeleteInstance', 'pending', ?1, 'restart cleanup scheduled', dispatched_at IS NOT NULL AND returned_at IS NULL, 1, ?1 FROM allocations WHERE tombstone=1 AND state='deleting' AND NOT EXISTS(SELECT 1 FROM allocation_actions action WHERE action.allocation_id=allocations.allocation_id AND action.reusable=1)",
            [now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Schedule cleanup after a dispatched create fails to produce a usable result.
    ///
    /// # Errors
    /// Returns `JournalError` if storage fails or the allocation is missing.
    pub fn schedule_allocation_cleanup(
        &self,
        allocation_id: &str,
        detail: &str,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        let now = Self::now() as i64;
        let changed = tx.execute(
            "UPDATE allocations SET tombstone=1, state=CASE WHEN dispatched_at IS NULL THEN 'deleted' ELSE 'deleting' END, detail=?2 WHERE allocation_id=?1 AND state!='deleted'",
            params![allocation_id, detail],
        )?;
        if changed == 0 {
            let exists = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM allocations WHERE allocation_id=?1)",
                [allocation_id],
                |row| row.get::<_, bool>(0),
            )?;
            if !exists {
                return Err(rusqlite::Error::QueryReturnedNoRows.into());
            }
        }
        tx.execute(
            "INSERT INTO allocation_actions(allocation_id, command, state, started_at, completed_at, detail, unresolved_create, reusable, next_attempt_at) SELECT allocation_id, 'DeleteInstance', CASE WHEN dispatched_at IS NULL THEN 'complete' ELSE 'pending' END, ?2, CASE WHEN dispatched_at IS NULL THEN ?2 ELSE NULL END, ?3, dispatched_at IS NOT NULL AND returned_at IS NULL, 1, ?2 FROM allocations WHERE allocation_id=?1 AND state!='deleted' AND NOT EXISTS(SELECT 1 FROM allocation_actions action WHERE action.allocation_id=allocations.allocation_id AND action.reusable=1)",
            params![allocation_id, now, detail],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// List tombstoned allocations awaiting guarded child cleanup.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn recoverable_allocations(&self) -> Result<Vec<AllocationRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE tombstone=1 AND state='deleting' AND NOT EXISTS(SELECT 1 FROM allocation_actions action WHERE action.allocation_id=allocations.allocation_id AND action.reusable=1 AND action.command='DeleteInstance' AND action.next_attempt_at>?1) ORDER BY created_at, allocation_id",
        )?;
        let rows = statement.query_map([Self::now() as i64], Self::read_allocation)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Read one allocation's retained cleanup scope without scheduling work.
    ///
    /// # Errors
    /// Returns `JournalError` when the journal or retained snapshot cannot be read.
    pub fn cleanup_resolution_scope(
        &self,
        allocation_id: &str,
    ) -> Result<Option<CleanupResolutionScope>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Self::read_cleanup_resolution_scope(&connection, allocation_id)
    }

    /// Check whether this exact attestation already committed for an allocation.
    ///
    /// # Errors
    /// Returns `JournalError` when the audit journal cannot be read.
    pub fn cleanup_resolution_matches(
        &self,
        allocation_id: &str,
        attestation_digest: &str,
    ) -> Result<bool, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM cleanup_resolution_audit audit JOIN allocations allocation USING(allocation_id) WHERE audit.allocation_id=?1 AND audit.attestation_digest=?2 AND allocation.tombstone=1 AND allocation.state='deleted')",
            params![allocation_id, attestation_digest],
            |row| row.get(0),
        )?)
    }

    /// Read the committed safe inspection summary for a resolved allocation.
    ///
    /// # Errors
    /// Returns `JournalError` when the append-only audit cannot be read.
    pub fn cleanup_resolution_summary_json(
        &self,
        allocation_id: &str,
    ) -> Result<Option<String>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(connection
            .query_row(
                "SELECT audit.summary_json FROM cleanup_resolution_audit audit JOIN allocations allocation USING(allocation_id) WHERE audit.allocation_id=?1 AND allocation.tombstone=1 AND allocation.state='deleted'",
                [allocation_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    fn read_cleanup_resolution_scope(
        connection: &Connection,
        allocation_id: &str,
    ) -> Result<Option<CleanupResolutionScope>, JournalError> {
        let allocation = connection
            .query_row(
                "SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE allocation_id=?1",
                [allocation_id],
                Self::read_allocation,
            )
            .optional()?;
        let Some(allocation) = allocation else {
            return Ok(None);
        };
        let snapshot_json = connection.query_row(
            "SELECT config_json FROM pool_snapshots WHERE id=?1",
            [&allocation.snapshot_id],
            |row| row.get(0),
        )?;
        let mut action_statement = connection.prepare(
            "SELECT action_id, command, state, started_at, completed_at, attempt_count, last_attempt_at, next_attempt_at, unresolved_create, reusable FROM allocation_actions WHERE allocation_id=?1 ORDER BY action_id",
        )?;
        let actions = action_statement
            .query_map([allocation_id], |row| {
                Ok(CleanupGeneration {
                    action_id: row.get(0)?,
                    command: row.get(1)?,
                    state: row.get(2)?,
                    started_at: u64::try_from(row.get::<_, i64>(3)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    completed_at: row
                        .get::<_, Option<i64>>(4)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    attempt_count: u32::try_from(row.get::<_, i64>(5)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    last_attempt_at: row
                        .get::<_, Option<i64>>(6)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    next_attempt_at: row
                        .get::<_, Option<i64>>(7)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    unresolved_create: row.get::<_, i64>(8)? != 0,
                    reusable: row.get::<_, i64>(9)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut attempt_statement = connection.prepare(
            "SELECT allocation_id, attempt_number, state, offer_json, binding_json, detail, created_at, dispatched_at, completed_at FROM child_attempts WHERE allocation_id=?1 ORDER BY attempt_number",
        )?;
        let child_attempts = attempt_statement
            .query_map([allocation_id], |row| {
                Ok(ChildAttemptRecord {
                    allocation_id: row.get(0)?,
                    attempt_number: u32::try_from(row.get::<_, i64>(1)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    state: row.get(2)?,
                    offer_json: row.get(3)?,
                    binding_json: row.get(4)?,
                    detail: row.get(5)?,
                    created_at: u64::try_from(row.get::<_, i64>(6)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    dispatched_at: row
                        .get::<_, Option<i64>>(7)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    completed_at: row
                        .get::<_, Option<i64>>(8)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(CleanupResolutionScope {
            allocation,
            snapshot_json,
            actions,
            child_attempts,
        }))
    }

    /// Commit operator-attested cleanup and its audit record atomically.
    ///
    /// # Errors
    /// Returns `JournalError` when the attestation is stale, conflicts, or storage fails.
    pub fn commit_cleanup_resolution(
        &self,
        allocation_id: &str,
        expected_scope_digest: &str,
        audit: &CleanupResolutionAudit,
        summary_json: &str,
    ) -> Result<bool, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        let existing: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT attestation_digest, evidence_reference, verified_at FROM cleanup_resolution_audit WHERE allocation_id=?1",
                [allocation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((digest, evidence, verified_at)) = existing {
            let same = digest == audit.attestation_digest
                && evidence == audit.evidence_reference
                && verified_at == i64::try_from(audit.verified_at).unwrap_or(-1);
            let terminal: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM allocations WHERE allocation_id=?1 AND tombstone=1 AND state='deleted')",
                [allocation_id],
                |row| row.get(0),
            )?;
            if same && terminal {
                tx.commit()?;
                return Ok(false);
            }
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        let scope = Self::read_cleanup_resolution_scope(&tx, allocation_id)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        if crate::cleanup::scope_digest(&scope).map_err(|_| rusqlite::Error::InvalidQuery)?
            != expected_scope_digest
            || !scope.allocation.tombstone
            || scope.allocation.state != "deleting"
            || scope.allocation.dispatched_at.is_none()
            || scope.actions.is_empty()
        {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        let now = Self::now();
        tx.execute(
            "INSERT INTO cleanup_resolution_audit(allocation_id, attestation_digest, evidence_reference, verified_at, committed_at, uid, gid, summary_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![allocation_id, audit.attestation_digest, audit.evidence_reference, i64::try_from(audit.verified_at).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::try_from(now).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::from(audit.uid), i64::from(audit.gid), summary_json],
        )?;
        tx.execute(
            "UPDATE allocations SET state='deleted' WHERE allocation_id=?1 AND tombstone=1 AND state='deleting'",
            [allocation_id],
        )?;
        if tx.changes() != 1 {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        tx.commit()?;
        Ok(true)
    }
}
