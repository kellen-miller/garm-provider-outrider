use super::{
    AllocationActionAdmission, AllocationActionRecord, Journal, JournalError,
    cleanup_backoff_seconds, native_identity_key,
};
use rusqlite::{OptionalExtension, params};

impl Journal {
    /// List startup-orphaned Start and Stop intents whose runners remain healthy.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure or invalid durable timestamps.
    pub fn recoverable_lifecycle_actions(
        &self,
    ) -> Result<Vec<AllocationActionRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let mut statement = connection.prepare(
            "SELECT action_id, allocation_id, command, state, started_at, completed_at, detail, unresolved_create FROM allocation_actions action WHERE command IN ('StartInstance','StopInstance') AND state='ambiguous' AND action_id=(SELECT MAX(latest.action_id) FROM allocation_actions latest WHERE latest.allocation_id=action.allocation_id) AND EXISTS(SELECT 1 FROM allocations allocation WHERE allocation.allocation_id=action.allocation_id AND allocation.state='created' AND allocation.tombstone=0) ORDER BY action_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(AllocationActionRecord {
                action_id: row.get(0)?,
                allocation_id: row.get(1)?,
                command: row.get(2)?,
                state: row.get(3)?,
                started_at: u64::try_from(row.get::<_, i64>(4)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                completed_at: row
                    .get::<_, Option<i64>>(5)?
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                detail: row.get(6)?,
                unresolved_create: row.get::<_, i64>(7)? != 0,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Durably admit one lifecycle action against the logical allocation row.
    ///
    /// Delete admission writes its tombstone in the same transaction. Start and
    /// stop admission fails while a delete or unresolved action owns the row.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn begin_allocation_action(
        &self,
        controller_id: &str,
        id_or_name: &str,
        command: &str,
    ) -> Result<Option<AllocationActionAdmission>, JournalError> {
        if !matches!(command, "DeleteInstance" | "StartInstance" | "StopInstance") {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        let allocation = tx
            .query_row(
                "SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE controller_id=?1 AND (allocation_id=?2 OR runner_name=?2)",
                params![controller_id, id_or_name],
                Self::read_allocation,
            )
            .optional()?;
        let Some(allocation) = allocation else {
            tx.commit()?;
            return Ok(None);
        };
        let active_action: Option<i64> = tx
            .query_row(
                "SELECT action_id FROM allocation_actions WHERE allocation_id=?1 AND state='intent' ORDER BY action_id LIMIT 1",
                [&allocation.allocation_id],
                |row| row.get(0),
            )
            .optional()?;
        let pending_delete: Option<(i64, String, i64)> = tx
            .query_row(
                "SELECT action_id, state, COALESCE(next_attempt_at, 0) FROM allocation_actions WHERE allocation_id=?1 AND command='DeleteInstance' AND reusable=1",
                [&allocation.allocation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        // A persisted positive child return closes Create uncertainty even if its
        // native ownership or subsequent cleanup has not yet been established.
        let unresolved_create =
            allocation.dispatched_at.is_some() && allocation.returned_at.is_none();
        let mut action_id = None;
        let mut acquired = false;
        if command == "DeleteInstance" && allocation.state != "deleted" {
            let now = Self::now() as i64;
            tx.execute(
                "UPDATE allocations SET tombstone=1, state=CASE WHEN dispatched_at IS NULL THEN 'deleted' ELSE 'deleting' END WHERE allocation_id=?1",
                [&allocation.allocation_id],
            )?;
            if pending_delete.is_none() {
                let (state, completed_at, detail) = if allocation.dispatched_at.is_none() {
                    ("complete", Some(now), None)
                } else if active_action.is_some() {
                    ("pending", None, Some("waiting for active lifecycle action"))
                } else {
                    ("pending", None, None)
                };
                tx.execute(
                    "INSERT INTO allocation_actions(allocation_id, command, state, started_at, completed_at, detail, unresolved_create, reusable, attempt_count, next_attempt_at) VALUES (?1, 'DeleteInstance', ?2, ?3, ?4, ?5, ?6, 1, 0, ?3)",
                    params![allocation.allocation_id, state, now, completed_at, detail, unresolved_create],
                )?;
            }
            let (delete_id, delete_state, next_attempt): (i64, String, i64) = tx.query_row(
                "SELECT action_id, state, COALESCE(next_attempt_at, 0) FROM allocation_actions WHERE allocation_id=?1 AND command='DeleteInstance' AND reusable=1",
                [&allocation.allocation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            action_id = Some(delete_id);
            if allocation.dispatched_at.is_none() {
                tx.execute(
                    "UPDATE allocation_actions SET state='complete', completed_at=?2 WHERE action_id=?1 AND reusable=1",
                    params![delete_id, now],
                )?;
            } else if active_action.is_none() && delete_state == "pending" && next_attempt <= now {
                tx.execute(
                    "UPDATE allocation_actions SET state='intent', completed_at=NULL, detail=NULL, unresolved_create=?2, attempt_count=attempt_count+1, last_attempt_at=?3, next_attempt_at=NULL WHERE action_id=?1 AND reusable=1 AND state='pending' AND COALESCE(next_attempt_at, 0)<=?3",
                    params![delete_id, unresolved_create, now],
                )?;
                acquired = tx.changes() == 1;
            }
        } else if allocation.state == "created"
            && !allocation.tombstone
            && active_action.is_none()
            && pending_delete.is_none()
            && !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM allocation_actions WHERE allocation_id=?1 AND state='ambiguous')",
                [&allocation.allocation_id],
                |row| row.get::<_, bool>(0),
            )?
        {
            tx.execute(
                "INSERT INTO allocation_actions(allocation_id, command, state, started_at, detail, unresolved_create) VALUES (?1, ?2, 'intent', ?3, NULL, 0)",
                params![allocation.allocation_id, command, Self::now() as i64],
            )?;
            action_id = Some(tx.last_insert_rowid());
            acquired = true;
        }
        let current = tx.query_row(
            "SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE allocation_id=?1",
            [&allocation.allocation_id],
            Self::read_allocation,
        )?;
        tx.commit()?;
        Ok(Some(AllocationActionAdmission {
            allocation: current,
            action_id,
            acquired,
            unresolved_create,
        }))
    }

    /// Finish or fence the latest lifecycle action after observing its outcome.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn finish_allocation_action(
        &self,
        action_id: i64,
        command: &str,
        state: &str,
        detail: Option<&str>,
    ) -> Result<bool, JournalError> {
        if !matches!(state, "complete" | "ambiguous") {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        if command == "DeleteInstance" {
            let tx = connection.unchecked_transaction()?;
            let attempt: Option<(i64, Option<i64>)> = tx
                .query_row(
                    "SELECT attempt_count, next_attempt_at FROM allocation_actions WHERE action_id=?1 AND command=?2 AND state='intent' AND reusable=1",
                    params![action_id, command],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((attempts, evidence_due)) = attempt else {
                tx.commit()?;
                return Ok(false);
            };
            let now = Self::now() as i64;
            let (stored_state, due) =
                if state == "complete" {
                    ("complete", None)
                } else {
                    (
                        "pending",
                        Some(evidence_due.unwrap_or_else(|| {
                            now.saturating_add(cleanup_backoff_seconds(attempts))
                        })),
                    )
                };
            tx.execute(
                "UPDATE allocation_actions SET state=?3, completed_at=?6, detail=?4, next_attempt_at=?5 WHERE action_id=?1 AND command=?2 AND state='intent' AND reusable=1",
                params![action_id, command, stored_state, detail, due, now],
            )?;
            let changed = tx.changes() == 1;
            tx.commit()?;
            return Ok(changed);
        }
        let changed = connection.execute(
            "UPDATE allocation_actions SET state=?3, completed_at=?4, detail=?5 WHERE action_id=?1 AND command=?2 AND state='intent'",
            params![action_id, command, state, Self::now() as i64, detail],
        )?;
        Ok(changed == 1)
    }

    /// Finish a startup-orphaned lifecycle action after read-only state reconciliation.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn finish_recovered_lifecycle_action(
        &self,
        action_id: i64,
        command: &str,
        state: &str,
        detail: Option<&str>,
    ) -> Result<bool, JournalError> {
        if !matches!(command, "StartInstance" | "StopInstance")
            || !matches!(state, "complete" | "ambiguous")
        {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let changed = connection.execute(
            "UPDATE allocation_actions SET state=?3, completed_at=?4, detail=?5 WHERE action_id=?1 AND command=?2 AND state='ambiguous'",
            params![action_id, command, state, Self::now() as i64, detail],
        )?;
        Ok(changed == 1)
    }

    /// Claim a newly observed physical resource identity before child mutation.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn claim_allocation_native_identity(
        &self,
        allocation_id: &str,
        identity: &crate::cloud::PlacementIdentity,
    ) -> Result<bool, JournalError> {
        let identity_json =
            serde_json::to_string(identity).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let identity_key = native_identity_key(identity);
        let mut connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.transaction()?;
        let had_identity: bool = tx
            .query_row(
                "SELECT native_identity_json IS NOT NULL FROM allocations WHERE allocation_id=?1",
                [allocation_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(false);
        let changed = match tx.execute(
            "UPDATE allocations SET native_identity_json=?2, native_identity_key=?3 WHERE allocation_id=?1 AND state IN ('dispatched','ambiguous','created','deleting') AND (native_identity_key IS NULL OR native_identity_key=?3) AND (native_identity_json IS NULL OR native_identity_json=?2) AND NOT EXISTS(SELECT 1 FROM allocations WHERE native_identity_key=?3 AND allocation_id!=?1)",
            params![allocation_id, identity_json, identity_key],
        ) {
            Ok(changed) => changed == 1,
            Err(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::ConstraintViolation => {
                tx.rollback()?;
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        if changed && !had_identity {
            tx.execute(
                "UPDATE allocation_actions SET next_attempt_at=?2 WHERE allocation_id=?1 AND command='DeleteInstance' AND reusable=1 AND state IN ('pending','intent')",
                params![allocation_id, Self::now() as i64],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Read the durable lifecycle action for one allocation.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure or invalid timestamps.
    pub fn allocation_action(
        &self,
        allocation_id: &str,
    ) -> Result<Option<AllocationActionRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(connection
            .query_row(
                "SELECT action_id, allocation_id, command, state, started_at, completed_at, detail, unresolved_create FROM allocation_actions WHERE allocation_id=?1 ORDER BY action_id DESC LIMIT 1",
                [allocation_id],
                |row| {
                    Ok(AllocationActionRecord {
                        action_id: row.get(0)?,
                        allocation_id: row.get(1)?,
                        command: row.get(2)?,
                        state: row.get(3)?,
                        started_at: u64::try_from(row.get::<_, i64>(4)?)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                        completed_at: row
                            .get::<_, Option<i64>>(5)?
                            .map(u64::try_from)
                            .transpose()
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                        detail: row.get(6)?,
                        unresolved_create: row.get::<_, i64>(7)? != 0,
                    })
                },
            )
            .optional()?)
    }

    /// Read the first durable DeleteInstance intent for one allocation.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure or an invalid timestamp.
    pub fn first_allocation_delete_intent(
        &self,
        allocation_id: &str,
    ) -> Result<Option<u64>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection
            .query_row(
                "SELECT MIN(started_at) FROM allocation_actions WHERE allocation_id=?1 AND command='DeleteInstance'",
                [allocation_id],
                |row| {
                    row.get::<_, Option<i64>>(0)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| rusqlite::Error::InvalidQuery)
                },
            )
            .map_err(Into::into)
    }
}
