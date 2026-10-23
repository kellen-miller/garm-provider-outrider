use super::{AllocationRecord, Journal, JournalError, OperationRecord, SystemTime, UNIX_EPOCH};
use rusqlite::{OptionalExtension, params};

impl Journal {
    /// Record an admitted provider operation before its side effects begin.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal lock is poisoned or a SQLite operation
    /// fails.
    pub fn admit(
        &self,
        controller: &str,
        instance: &str,
        command: &str,
    ) -> Result<OperationRecord, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        let previous = tx.query_row(
            "SELECT controller_id, instance_id, command, state, tombstone, detail FROM operations WHERE controller_id=?1 AND instance_id=?2",
            params![controller, instance], Self::read_record,
        ).optional()?;
        let record = if let Some(previous) = previous {
            if command == "DeleteInstance" && !previous.tombstone {
                tx.execute("UPDATE operations SET command=?3, state='admitted', tombstone=1, detail=NULL, updated_at=?4 WHERE controller_id=?1 AND instance_id=?2",
                    params![controller, instance, command, Self::now() as i64])?;
                OperationRecord {
                    controller_id: controller.into(),
                    instance_id: instance.into(),
                    command: command.into(),
                    state: "admitted".into(),
                    tombstone: true,
                    detail: None,
                }
            } else if previous.tombstone || previous.state == "admitted" {
                previous
            } else {
                let tombstone = command == "DeleteInstance" || command == "RemoveAllInstances";
                tx.execute("UPDATE operations SET command=?3, state='admitted', tombstone=?4, detail=NULL, updated_at=?5 WHERE controller_id=?1 AND instance_id=?2",
                    params![controller, instance, command, tombstone, Self::now() as i64])?;
                OperationRecord {
                    controller_id: controller.into(),
                    instance_id: instance.into(),
                    command: command.into(),
                    state: "admitted".into(),
                    tombstone,
                    detail: None,
                }
            }
        } else {
            let tombstone = command == "DeleteInstance" || command == "RemoveAllInstances";
            tx.execute("INSERT INTO operations(controller_id, instance_id, command, state, tombstone, updated_at) VALUES (?1, ?2, ?3, 'admitted', ?4, ?5)",
                params![controller, instance, command, tombstone, Self::now() as i64])?;
            OperationRecord {
                controller_id: controller.into(),
                instance_id: instance.into(),
                command: command.into(),
                state: "admitted".into(),
                tombstone,
                detail: None,
            }
        };

        tx.commit()?;
        Ok(record)
    }

    /// Mark an admitted operation unresolved when its executor is unavailable.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal lock is poisoned or the update fails.
    pub fn mark_unavailable(
        &self,
        controller: &str,
        instance: &str,
        message: &str,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute("UPDATE operations SET state='unresolved', detail=?3, updated_at=?4 WHERE controller_id=?1 AND instance_id=?2",
            params![controller, instance, message, Self::now() as i64])?;
        Ok(())
    }

    /// Read the operation recorded for one controller and instance.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal lock is poisoned or the query fails.
    pub fn get(
        &self,
        controller: &str,
        instance: &str,
    ) -> Result<Option<OperationRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(connection.query_row("SELECT controller_id, instance_id, command, state, tombstone, detail FROM operations WHERE controller_id=?1 AND instance_id=?2",
            params![controller, instance], Self::read_record).optional()?)
    }

    /// List admitted or unresolved operations for a controller.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal lock is poisoned or the query fails.
    pub fn unresolved(&self, controller: &str) -> Result<Vec<OperationRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let mut statement = connection.prepare("SELECT controller_id, instance_id, command, state, tombstone, detail FROM operations WHERE controller_id=?1 AND state IN ('admitted','unresolved') ORDER BY updated_at")?;
        let rows = statement.query_map(params![controller], Self::read_record)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Count active operations that must finish before a dynamic pool can be deleted.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn pool_outstanding_operations(&self, pool_id: &str) -> Result<usize, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM allocations WHERE pool_id=?1 AND state IN ('reserved','selecting','dispatched','ambiguous','created','deleting')",
            [pool_id],
            |row| row.get(0),
        )?;
        usize::try_from(count).map_err(|_| rusqlite::Error::InvalidQuery.into())
    }

    fn read_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<OperationRecord> {
        Ok(OperationRecord {
            controller_id: row.get(0)?,
            instance_id: row.get(1)?,
            command: row.get(2)?,
            state: row.get(3)?,
            tombstone: row.get(4)?,
            detail: row.get(5)?,
        })
    }

    pub(super) fn read_allocation(row: &rusqlite::Row<'_>) -> rusqlite::Result<AllocationRecord> {
        Ok(AllocationRecord {
            allocation_id: row.get(0)?,
            controller_id: row.get(1)?,
            runner_name: row.get(2)?,
            snapshot_id: row.get(3)?,
            catalog_fingerprint: row.get(4)?,
            pool_id: row.get(5)?,
            account_id: row.get(6)?,
            state: row.get(7)?,
            selected_offer_json: row.get(8)?,
            binding_json: row.get(9)?,
            provider_instance_json: row.get(10)?,
            native_identity_json: row.get(11)?,
            detail: row.get(12)?,
            created_at: u64::try_from(row.get::<_, i64>(13)?)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
            dispatched_at: row
                .get::<_, Option<i64>>(14)?
                .map(u64::try_from)
                .transpose()
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
            returned_at: row
                .get::<_, Option<i64>>(15)?
                .map(u64::try_from)
                .transpose()
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
            tombstone: row.get(16)?,
        })
    }

    pub(super) fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}
