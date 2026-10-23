use super::{Journal, JournalError, OutboxMessage, PoolSnapshot, WebhookDelivery};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

impl Journal {
    pub(super) fn sha256(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn digest_fields(domain: &[u8], fields: &[&[u8]]) -> String {
        let mut digest = Sha256::new();
        digest.update((domain.len() as u64).to_be_bytes());
        digest.update(domain);
        for field in fields {
            digest.update((field.len() as u64).to_be_bytes());
            digest.update(field);
        }

        hex::encode(digest.finalize())
    }

    pub(super) fn delivery_digest(
        delivery_id: &str,
        repository_id: i64,
        event: &str,
        headers_json: &str,
        body: &[u8],
    ) -> String {
        Self::digest_fields(
            b"outrider-webhook-delivery-v1",
            &[
                delivery_id.as_bytes(),
                &repository_id.to_be_bytes(),
                event.as_bytes(),
                headers_json.as_bytes(),
                body,
            ],
        )
    }

    pub(super) fn outbox_digest(
        id: &str,
        kind: &str,
        destination: &str,
        headers_json: &str,
        body: &[u8],
    ) -> String {
        Self::digest_fields(
            b"outrider-ingress-outbox-v1",
            &[
                id.as_bytes(),
                kind.as_bytes(),
                destination.as_bytes(),
                headers_json.as_bytes(),
                body,
            ],
        )
    }

    /// Store an authenticated webhook before returning an acknowledgment.
    /// Existing delivery IDs are idempotent only when their bytes and identity match.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn persist_delivery(&self, delivery: &WebhookDelivery) -> Result<bool, JournalError> {
        self.persist_delivery_with_terminal(delivery, None)
    }

    /// Persist a signed delivery and its workflow-run terminal/cancellation marker atomically.
    ///
    /// # Errors
    /// Returns `JournalError` when delivery identity conflicts or SQLite fails.
    pub fn persist_delivery_with_terminal(
        &self,
        delivery: &WebhookDelivery,
        terminal: Option<(u64, u32, bool)>,
    ) -> Result<bool, JournalError> {
        let mut connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let transaction = connection.transaction()?;
        let repository_id =
            i64::try_from(delivery.repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let digest = Self::delivery_digest(
            &delivery.delivery_id,
            repository_id,
            &delivery.event,
            &delivery.headers_json,
            &delivery.body,
        );
        let previous = transaction
            .query_row(
                "SELECT payload_sha256 FROM webhook_deliveries WHERE delivery_id=?1",
                params![delivery.delivery_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let inserted = if let Some(previous_digest) = previous {
            if previous_digest != digest {
                return Err(rusqlite::Error::InvalidQuery.into());
            }
            false
        } else {
            transaction.execute(
                "INSERT INTO webhook_deliveries(delivery_id, repository_id, event, headers_json, body, state, created_at, retry_order, payload_sha256, terminal_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, COALESCE((SELECT MAX(retry_order) + 1 FROM webhook_deliveries), 1), ?8, CASE WHEN ?6 IN ('queued','rejected') THEN ?7 ELSE NULL END)",
                params![delivery.delivery_id, repository_id, delivery.event, delivery.headers_json, delivery.body, delivery.state, Self::now() as i64, digest],
            )?;
            true
        };
        if let Some((run_id, attempt, cancelled)) = terminal {
            transaction.execute(
                "INSERT INTO terminal_runs(repository_id, run_id, run_attempt, completed_at, cancelled) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(repository_id, run_id, run_attempt) DO UPDATE SET cancelled=MAX(terminal_runs.cancelled, excluded.cancelled)",
                params![repository_id, i64::try_from(run_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::from(attempt), Self::now() as i64, i64::from(cancelled)],
            )?;
        }
        transaction.commit()?;
        Ok(inserted)
    }

    /// List durable deliveries still awaiting processing, oldest first.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn pending_deliveries(&self, limit: usize) -> Result<Vec<WebhookDelivery>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let mut statement = connection.prepare("SELECT delivery_id, repository_id, event, headers_json, body, state FROM webhook_deliveries WHERE state IN ('received','retry') ORDER BY retry_order, created_at, rowid LIMIT ?1")?;
        let rows = statement.query_map(params![limit as i64], |row| {
            Ok(WebhookDelivery {
                delivery_id: row.get(0)?,
                repository_id: u64::try_from(row.get::<_, i64>(1)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                event: row.get(2)?,
                headers_json: row.get(3)?,
                body: row.get(4)?,
                state: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Read one delivery state for deterministic recovery inspection.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn delivery_state(&self, delivery_id: &str) -> Result<Option<String>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(connection
            .query_row(
                "SELECT state FROM webhook_deliveries WHERE delivery_id=?1",
                params![delivery_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Update durable delivery state without deleting its signed bytes.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn set_delivery_state(
        &self,
        delivery_id: &str,
        state: &str,
        detail: Option<&str>,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        if state == "retry" {
            connection.execute(
                "UPDATE webhook_deliveries SET state=?2, detail=?3, retry_order=(SELECT COALESCE(MAX(retry_order),0)+1 FROM webhook_deliveries) WHERE delivery_id=?1 AND terminal_at IS NULL",
                params![delivery_id, state, detail],
            )?;
        } else {
            connection.execute(
                "UPDATE webhook_deliveries SET state=?2, detail=?3, terminal_at=CASE WHEN ?2 IN ('queued','rejected') THEN COALESCE(terminal_at, ?4) ELSE terminal_at END WHERE delivery_id=?1 AND (terminal_at IS NULL OR ?2 IN ('queued','rejected'))",
                params![delivery_id, state, detail, Self::now() as i64],
            )?;
        }
        Ok(())
    }

    /// Persist a relay or rejection-check intent before dispatching it.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn enqueue_outbox(&self, message: &OutboxMessage) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let digest = Self::outbox_digest(
            &message.id,
            &message.kind,
            &message.destination,
            &message.headers_json,
            &message.body,
        );
        let previous_digest = connection
            .query_row(
                "SELECT payload_sha256 FROM ingress_outbox WHERE id=?1",
                [&message.id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if previous_digest.is_some_and(|previous_digest| previous_digest != digest) {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        connection.execute("INSERT OR IGNORE INTO ingress_outbox(id, kind, destination, headers_json, body, state, created_at, retry_order, payload_sha256) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, COALESCE((SELECT MAX(retry_order) + 1 FROM ingress_outbox), 1), ?7)", params![message.id, message.kind, message.destination, message.headers_json, message.body, Self::now() as i64, digest])?;
        Ok(())
    }

    /// Read pending outbox work oldest first.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn pending_outbox(&self, limit: usize) -> Result<Vec<OutboxMessage>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let mut statement = connection.prepare("SELECT id, kind, destination, headers_json, body, state FROM ingress_outbox WHERE state IN ('pending','retry') ORDER BY retry_order, created_at, rowid LIMIT ?1")?;
        let rows = statement.query_map(params![limit as i64], |row| {
            Ok(OutboxMessage {
                id: row.get(0)?,
                kind: row.get(1)?,
                destination: row.get(2)?,
                headers_json: row.get(3)?,
                body: row.get(4)?,
                state: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Record the outcome of an outbox dispatch, preserving retry intent.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn set_outbox_state(
        &self,
        id: &str,
        state: &str,
        detail: Option<&str>,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        if state == "retry" {
            connection.execute(
                "UPDATE ingress_outbox SET state=?2, detail=?3, attempts=attempts+1, retry_order=(SELECT COALESCE(MAX(retry_order),0)+1 FROM ingress_outbox) WHERE id=?1 AND terminal_at IS NULL",
                params![id, state, detail],
            )?;
        } else {
            connection.execute(
                "UPDATE ingress_outbox SET state=?2, detail=?3, attempts=attempts+1, terminal_at=CASE WHEN ?2 IN ('delivered','rejected') THEN COALESCE(terminal_at, ?4) ELSE terminal_at END WHERE id=?1 AND (terminal_at IS NULL OR ?2 IN ('delivered','rejected'))",
                params![id, state, detail, Self::now() as i64],
            )?;
        }
        Ok(())
    }

    /// Read one outbox delivery state for recovery inspection.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite fails.
    pub fn outbox_state(&self, id: &str) -> Result<Option<String>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(connection
            .query_row(
                "SELECT state FROM ingress_outbox WHERE id=?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Persist an immutable request configuration and its pool lifecycle state.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn save_pool_snapshot(&self, snapshot: &PoolSnapshot) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let config_sha256 = Self::sha256(snapshot.config_json.as_bytes());
        let now = Self::now() as i64;
        let terminal_at = matches!(snapshot.state.as_str(), "deleted" | "rejected").then_some(now);
        connection.execute("INSERT INTO pool_snapshots(id, repository_id, request_token, config_json, state, garm_pool_id, expires_at, created_at, config_sha256, terminal_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) ON CONFLICT(repository_id, request_token) DO NOTHING", params![snapshot.id, i64::try_from(snapshot.repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?, snapshot.request_token, snapshot.config_json, snapshot.state, snapshot.garm_pool_id, i64::try_from(snapshot.expires_at).map_err(|_| rusqlite::Error::InvalidQuery)?, now, config_sha256, terminal_at])?;
        let stored: (String, String, i64) = connection.query_row(
            "SELECT id, config_json, expires_at FROM pool_snapshots WHERE repository_id=?1 AND request_token=?2",
            params![i64::try_from(snapshot.repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?, snapshot.request_token],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if stored.0 != snapshot.id
            || stored.1 != snapshot.config_json
            || stored.2
                != i64::try_from(snapshot.expires_at).map_err(|_| rusqlite::Error::InvalidQuery)?
        {
            return Err(JournalError::SnapshotConflict);
        }
        Ok(())
    }

    /// Read a snapshot by its stable request key.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn pool_snapshot(
        &self,
        repository_id: u64,
        request_token: &str,
    ) -> Result<Option<PoolSnapshot>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(connection.query_row("SELECT id, repository_id, request_token, config_json, state, garm_pool_id, expires_at FROM pool_snapshots WHERE repository_id=?1 AND request_token=?2", params![i64::try_from(repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?, request_token], |row| Ok(PoolSnapshot {
            id: row.get(0)?, repository_id: u64::try_from(row.get::<_, i64>(1)?).map_err(|_| rusqlite::Error::InvalidQuery)?, request_token: row.get(2)?, config_json: row.get(3)?, state: row.get(4)?, garm_pool_id: row.get(5)?, expires_at: u64::try_from(row.get::<_, i64>(6)?).map_err(|_| rusqlite::Error::InvalidQuery)?,
        })).optional()?)
    }

    /// Read an immutable pool snapshot by the ID carried by GARM.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn pool_snapshot_by_id(&self, id: &str) -> Result<Option<PoolSnapshot>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(connection.query_row("SELECT id, repository_id, request_token, config_json, state, garm_pool_id, expires_at FROM pool_snapshots WHERE id=?1", [id], |row| Ok(PoolSnapshot {
            id: row.get(0)?, repository_id: u64::try_from(row.get::<_, i64>(1)?).map_err(|_| rusqlite::Error::InvalidQuery)?, request_token: row.get(2)?, config_json: row.get(3)?, state: row.get(4)?, garm_pool_id: row.get(5)?, expires_at: u64::try_from(row.get::<_, i64>(6)?).map_err(|_| rusqlite::Error::InvalidQuery)?,
        })).optional()?)
    }

    /// Change pool snapshot state after a remote pool operation is reconciled.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn set_pool_snapshot_state(
        &self,
        id: &str,
        state: &str,
        pool_id: Option<&str>,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute("UPDATE pool_snapshots SET state=?2, garm_pool_id=COALESCE(?3, garm_pool_id), terminal_at=CASE WHEN ?2 IN ('deleted','rejected') THEN COALESCE(terminal_at, ?4) ELSE terminal_at END WHERE id=?1 AND (terminal_at IS NULL OR ?2 IN ('deleted','rejected'))", params![id, state, pool_id, Self::now() as i64])?;
        Ok(())
    }

    /// Count dynamic pools that are active or still draining.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn active_pool_snapshots(&self) -> Result<Vec<PoolSnapshot>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let mut statement = connection.prepare("SELECT id, repository_id, request_token, config_json, state, garm_pool_id, expires_at FROM pool_snapshots WHERE state NOT IN ('deleted','rejected') ORDER BY created_at")?;
        let rows = statement.query_map([], |row| {
            Ok(PoolSnapshot {
                id: row.get(0)?,
                repository_id: u64::try_from(row.get::<_, i64>(1)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                request_token: row.get(2)?,
                config_json: row.get(3)?,
                state: row.get(4)?,
                garm_pool_id: row.get(5)?,
                expires_at: u64::try_from(row.get::<_, i64>(6)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
