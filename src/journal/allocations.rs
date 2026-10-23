use super::{
    AllocationAdmission, AllocationDeleteTarget, AllocationRecord, ChildAttemptRecord, Journal,
    JournalError, RETAINED_CONFIG_ALLOCATION_PREDICATE, native_identity_key,
};
use rusqlite::{OptionalExtension, params};

impl Journal {
    /// Reserve one logical allocation before selecting or dispatching a child.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure. The returned row may be an existing
    /// same-name reservation and must be reconciled instead of dispatched again.
    pub fn reserve_allocation(
        &self,
        allocation_id: &str,
        controller_id: &str,
        runner_name: &str,
        snapshot_id: &str,
        pool_id: &str,
    ) -> Result<(AllocationRecord, bool), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        let existing = tx.query_row(
            "SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE controller_id=?1 AND runner_name=?2",
            params![controller_id, runner_name],
            Self::read_allocation,
        ).optional()?;
        if let Some(record) = existing {
            tx.commit()?;
            return Ok((record, false));
        }
        let deleted_identity: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM allocation_delete_fences WHERE controller_id=?1 AND identity_key IN (?2, ?3))",
            params![controller_id, runner_name, allocation_id],
            |row| row.get(0),
        )?;
        if deleted_identity {
            return Err(JournalError::AllocationIdentityDeleted);
        }
        let now = Self::now();
        tx.execute("INSERT INTO allocations(allocation_id, controller_id, runner_name, snapshot_id, pool_id, state, created_at) VALUES (?1, ?2, ?3, ?4, ?5, 'reserved', ?6)", params![allocation_id, controller_id, runner_name, snapshot_id, pool_id, now as i64])?;
        let record = tx.query_row(
            "SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE allocation_id=?1",
            [allocation_id],
            Self::read_allocation,
        )?;
        tx.commit()?;
        Ok((record, true))
    }

    /// Resolve one delete target or durably fence a missing identity atomically.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure or a known allocation pool mismatch.
    pub fn allocation_delete_target(
        &self,
        controller_id: &str,
        id_or_name: &str,
        pool_id: Option<&str>,
    ) -> Result<AllocationDeleteTarget, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        let allocation = tx
            .query_row(
                "SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE controller_id=?1 AND (allocation_id=?2 OR runner_name=?2)",
                params![controller_id, id_or_name],
                Self::read_allocation,
            )
            .optional()?;
        if let Some(allocation) = allocation {
            if pool_id.is_some_and(|pool| pool != allocation.pool_id) {
                return Err(JournalError::AllocationPoolMismatch);
            }
            tx.commit()?;
            return Ok(AllocationDeleteTarget::Owned(Box::new(allocation)));
        }
        tx.execute(
            "INSERT INTO allocation_delete_fences(controller_id, identity_key, created_at) VALUES (?1, ?2, ?3) ON CONFLICT(controller_id, identity_key) DO NOTHING",
            params![controller_id, id_or_name, Self::now() as i64],
        )?;
        tx.commit()?;
        Ok(AllocationDeleteTarget::Absent)
    }

    /// Read an allocation by either its opaque ID or GARM runner name.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn allocation(
        &self,
        controller_id: &str,
        id_or_name: &str,
    ) -> Result<Option<AllocationRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(connection.query_row("SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE controller_id=?1 AND (allocation_id=?2 OR runner_name=?2)", params![controller_id, id_or_name], Self::read_allocation).optional()?)
    }

    /// Read allocations retained for one controller and pool, including unresolved rows.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn allocations_for_pool(
        &self,
        controller_id: &str,
        pool_id: &str,
    ) -> Result<Vec<AllocationRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let mut statement = connection.prepare("SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE controller_id=?1 AND pool_id=?2 ORDER BY created_at")?;
        let rows = statement.query_map(params![controller_id, pool_id], Self::read_allocation)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Read every retained allocation owned by a controller.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn allocations_for_controller(
        &self,
        controller_id: &str,
    ) -> Result<Vec<AllocationRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let mut statement = connection.prepare("SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE controller_id=?1 ORDER BY created_at")?;
        let rows = statement.query_map([controller_id], Self::read_allocation)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Read retained allocations for restart-safe interruption identity history.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn all_allocations(&self) -> Result<Vec<AllocationRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let mut statement = connection.prepare("SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations ORDER BY created_at, allocation_id")?;
        let rows = statement.query_map([], Self::read_allocation)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Read created and cleanup-pending allocations eligible for read-only polling.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn observable_allocations(&self) -> Result<Vec<AllocationRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let mut statement = connection.prepare("SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE state IN ('created','deleting') AND native_identity_json IS NOT NULL ORDER BY created_at, allocation_id")?;
        let rows = statement.query_map([], Self::read_allocation)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Read one allocation by its durable opaque identity.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn allocation_by_id(
        &self,
        allocation_id: &str,
    ) -> Result<Option<AllocationRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(connection
            .query_row(
                "SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE allocation_id=?1",
                [allocation_id],
                Self::read_allocation,
            )
            .optional()?)
    }

    /// Persist the selected offer and binding before a child attempt exists.
    ///
    /// # Errors
    /// Returns `JournalError` when the allocation is missing or SQLite fails.
    pub fn set_allocation_selection(
        &self,
        allocation_id: &str,
        offer_json: &str,
        binding_json: &str,
        account_id: &str,
        catalog_fingerprint: &str,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute("UPDATE allocations SET selected_offer_json=?2, binding_json=?3, account_id=?4, catalog_fingerprint=?5 WHERE allocation_id=?1 AND state='selecting'", params![allocation_id, offer_json, binding_json, account_id, catalog_fingerprint])?;
        Ok(())
    }

    /// Close a definitive pre-spawn failure without releasing a raced dispatch or tombstone.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn close_unspawned_allocation(&self, allocation_id: &str) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute("UPDATE allocations SET state='no_candidate' WHERE allocation_id=?1 AND dispatched_at IS NULL AND tombstone=0 AND state IN ('reserved','selecting','no_candidate')", [allocation_id])?;
        Ok(())
    }

    /// Claim a reserved logical allocation for one in-process selector.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn claim_allocation_selection(&self, allocation_id: &str) -> Result<bool, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let changed = connection.execute("UPDATE allocations SET state='selecting' WHERE allocation_id=?1 AND state IN ('reserved','no_candidate')", [allocation_id])?;
        Ok(changed == 1)
    }

    /// Release selectors left open by a process crash before the durable dispatch marker.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn reset_interrupted_selection(&self) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute("UPDATE allocations SET state='reserved', selected_offer_json=NULL, binding_json=NULL, account_id=NULL, catalog_fingerprint=NULL WHERE state='selecting'", [])?;
        Ok(())
    }

    /// Insert a durable child attempt before spawning its pinned process.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail.
    pub fn insert_child_attempt(&self, attempt: &ChildAttemptRecord) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute("INSERT INTO child_attempts(allocation_id, attempt_number, state, offer_json, binding_json, detail, created_at) VALUES (?1, ?2, 'reserved', ?3, ?4, NULL, ?5)", params![attempt.allocation_id, i64::from(attempt.attempt_number), attempt.offer_json, attempt.binding_json, attempt.created_at as i64])?;
        Ok(())
    }

    /// Mark allocation and attempt dispatched before the child process is spawned.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail.
    pub fn mark_child_dispatched(
        &self,
        allocation_id: &str,
        attempt_number: u32,
        admission: &AllocationAdmission,
    ) -> Result<bool, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        let now = Self::now() as i64;
        let allocation_changed = tx.execute("UPDATE allocations SET state='dispatched', dispatched_at=?2 WHERE allocation_id=?1 AND state='selecting' AND tombstone=0 AND EXISTS (SELECT 1 FROM pool_snapshots WHERE id=?3 AND state='enabled' AND expires_at>?4) AND NOT EXISTS (SELECT 1 FROM terminal_runs WHERE repository_id=?5 AND run_id=?6 AND run_attempt=?7)", params![allocation_id, now, admission.snapshot_id, admission.now as i64, i64::try_from(admission.repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::try_from(admission.run_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::from(admission.run_attempt)])?;
        let attempt_changed = if allocation_changed == 1 {
            tx.execute("UPDATE child_attempts SET state='dispatched', dispatched_at=?3 WHERE allocation_id=?1 AND attempt_number=?2 AND state='reserved'", params![allocation_id, i64::from(attempt_number), now])?
        } else {
            0
        };
        if allocation_changed != 1 || attempt_changed != 1 {
            tx.rollback()?;
            return Ok(false);
        }
        tx.commit()?;
        Ok(true)
    }

    /// Persist a definitive local attempt failure or an ambiguous child result.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail.
    pub fn finish_child_attempt(
        &self,
        allocation_id: &str,
        attempt_number: u32,
        state: &str,
        detail: &str,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute("UPDATE child_attempts SET state=?3, detail=?4, completed_at=?5 WHERE allocation_id=?1 AND attempt_number=?2", params![allocation_id, i64::from(attempt_number), state, detail, Self::now() as i64])?;
        Ok(())
    }

    /// Store the child response and native identity after a successful create.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail.
    pub fn complete_allocation(
        &self,
        allocation_id: &str,
        provider_instance_json: &str,
        native_identity_json: &str,
    ) -> Result<bool, JournalError> {
        let identity: crate::cloud::PlacementIdentity = serde_json::from_str(native_identity_json)
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let identity_key = native_identity_key(&identity);
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        match connection.execute("UPDATE allocations SET state='created', provider_instance_json=?2, native_identity_json=?3, native_identity_key=?4, returned_at=?5, detail=NULL WHERE allocation_id=?1 AND state IN ('dispatched','ambiguous') AND tombstone=0", params![allocation_id, provider_instance_json, native_identity_json, identity_key, Self::now() as i64]) {
            Ok(changed) => Ok(changed == 1),
            Err(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::ConstraintViolation => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Set an allocation's lifecycle state after an independently verified action.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail.
    pub fn set_allocation_state(
        &self,
        allocation_id: &str,
        state: &str,
        detail: Option<&str>,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute(
            "UPDATE allocations SET state=?2, detail=?3 WHERE allocation_id=?1",
            params![allocation_id, state, detail],
        )?;
        Ok(())
    }

    /// Persist sanitized allocation diagnostics without changing its state.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn set_allocation_detail(
        &self,
        allocation_id: &str,
        detail: Option<&str>,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute(
            "UPDATE allocations SET detail=?2 WHERE allocation_id=?1",
            params![allocation_id, detail],
        )?;
        Ok(())
    }

    /// Keep a dispatched allocation fenced when the child outcome is uncertain.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail.
    pub fn mark_allocation_ambiguous(
        &self,
        allocation_id: &str,
        detail: &str,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute("UPDATE allocations SET state='ambiguous', detail=?2 WHERE allocation_id=?1 AND state IN ('dispatched','ambiguous')", params![allocation_id, detail])?;
        Ok(())
    }

    /// Preserve a successful child locator while native ownership remains unresolved.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail.
    pub fn preserve_ambiguous_child_result(
        &self,
        allocation_id: &str,
        provider_instance_json: &str,
        detail: &str,
    ) -> Result<(), JournalError> {
        let mut connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.transaction()?;
        let now = Self::now() as i64;
        let changed = tx.execute(
            "UPDATE allocations SET state=CASE WHEN tombstone=1 THEN 'deleting' ELSE 'ambiguous' END, provider_instance_json=?2, detail=?3, returned_at=?4 WHERE allocation_id=?1 AND dispatched_at IS NOT NULL AND returned_at IS NULL AND state IN ('dispatched','ambiguous','deleting')",
            params![allocation_id, provider_instance_json, detail, now],
        )?;
        if changed == 1 {
            tx.execute(
                "UPDATE allocation_actions SET next_attempt_at=?2 WHERE allocation_id=?1 AND command='DeleteInstance' AND reusable=1 AND state IN ('pending','intent')",
                params![allocation_id, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Reset an allocation after a proven pre-spawn local failure.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail.
    pub fn reset_local_allocation(&self, allocation_id: &str) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute("UPDATE allocations SET state='selecting', selected_offer_json=NULL, binding_json=NULL, account_id=NULL, catalog_fingerprint=NULL, dispatched_at=NULL WHERE allocation_id=?1 AND state='dispatched' AND NOT EXISTS (SELECT 1 FROM child_attempts WHERE allocation_id=?1 AND state IN ('ambiguous','created'))", [allocation_id])?;
        Ok(())
    }

    /// Count unresolved and live allocations against controller, pool and account caps.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail.
    pub fn active_allocation_counts(
        &self,
        controller_id: &str,
        pool_id: &str,
    ) -> Result<(usize, usize), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let controller_count = connection.query_row("SELECT COUNT(*) FROM allocations WHERE controller_id=?1 AND state IN ('reserved','selecting','dispatched','ambiguous','created','deleting')", [controller_id], |row| row.get::<_, i64>(0))?;
        let pool_count = connection.query_row("SELECT COUNT(*) FROM allocations WHERE controller_id=?1 AND pool_id=?2 AND state IN ('reserved','selecting','dispatched','ambiguous','created','deleting')", params![controller_id, pool_id], |row| row.get::<_, i64>(0))?;
        Ok((
            usize::try_from(controller_count).map_err(|_| rusqlite::Error::InvalidQuery)?,
            usize::try_from(pool_count).map_err(|_| rusqlite::Error::InvalidQuery)?,
        ))
    }

    /// Count the active allocations in all three required cap scopes.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail.
    pub fn active_allocation_counts_for_account(
        &self,
        controller_id: &str,
        pool_id: &str,
        account_id: &str,
    ) -> Result<(usize, usize, usize), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let controller_count = connection.query_row("SELECT COUNT(*) FROM allocations WHERE controller_id=?1 AND state IN ('reserved','selecting','dispatched','ambiguous','created','deleting')", [controller_id], |row| row.get::<_, i64>(0))?;
        let pool_count = connection.query_row("SELECT COUNT(*) FROM allocations WHERE controller_id=?1 AND pool_id=?2 AND state IN ('reserved','selecting','dispatched','ambiguous','created','deleting')", params![controller_id, pool_id], |row| row.get::<_, i64>(0))?;
        let account_count = connection.query_row("SELECT COUNT(*) FROM allocations WHERE controller_id=?1 AND account_id=?2 AND state IN ('reserved','selecting','dispatched','ambiguous','created','deleting')", params![controller_id, account_id], |row| row.get::<_, i64>(0))?;
        Ok((
            usize::try_from(controller_count).map_err(|_| rusqlite::Error::InvalidQuery)?,
            usize::try_from(pool_count).map_err(|_| rusqlite::Error::InvalidQuery)?,
            usize::try_from(account_count).map_err(|_| rusqlite::Error::InvalidQuery)?,
        ))
    }

    /// Write a durable delete tombstone by the same logical allocation identity.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn tombstone_allocation(
        &self,
        controller_id: &str,
        id_or_name: &str,
    ) -> Result<Option<AllocationRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        tx.execute("UPDATE allocations SET tombstone=1, state=CASE WHEN state='created' THEN 'deleting' ELSE state END WHERE controller_id=?1 AND (allocation_id=?2 OR runner_name=?2)", params![controller_id, id_or_name])?;
        let record = tx.query_row("SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE controller_id=?1 AND (allocation_id=?2 OR runner_name=?2)", params![controller_id, id_or_name], Self::read_allocation).optional()?;
        tx.commit()?;
        Ok(record)
    }

    /// Read all durable child attempts for one allocation in dispatch order.
    ///
    /// # Errors
    /// Returns `JournalError` on storage failure.
    pub fn child_attempts(
        &self,
        allocation_id: &str,
    ) -> Result<Vec<ChildAttemptRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let mut statement = connection.prepare("SELECT allocation_id, attempt_number, state, offer_json, binding_json, detail, created_at, dispatched_at, completed_at FROM child_attempts WHERE allocation_id=?1 ORDER BY attempt_number")?;
        let rows = statement.query_map([allocation_id], |row| {
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
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Read dispatched allocations that still require their immutable catalog configuration.
    ///
    /// # Errors
    /// Returns `JournalError` when the allocation journal cannot be read.
    pub fn allocations_requiring_retained_config(
        &self,
    ) -> Result<Vec<AllocationRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let retained_allocations = format!(
            "SELECT allocation_id, controller_id, runner_name, snapshot_id, catalog_fingerprint, pool_id, account_id, state, selected_offer_json, binding_json, provider_instance_json, native_identity_json, detail, created_at, dispatched_at, returned_at, tombstone FROM allocations WHERE {RETAINED_CONFIG_ALLOCATION_PREDICATE} ORDER BY created_at, allocation_id"
        );
        let mut statement = connection.prepare(&retained_allocations)?;
        let rows = statement.query_map([], Self::read_allocation)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
