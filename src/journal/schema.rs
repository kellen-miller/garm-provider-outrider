use super::{Journal, JournalError, JournalLock, Mutex, OpenOptions, native_identity_key};
use fs2::FileExt;
use rusqlite::{Connection, params};
use std::{fs, path::Path};

impl Journal {
    /// Open the SQLite journal and acquire its exclusive process lock.
    ///
    /// # Errors
    ///
    /// Returns an error if the lock is held, the database cannot be opened or
    /// initialized, or its schema version is unsupported.
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent)?;
        }
        let database_path = match fs::symlink_metadata(path) {
            Ok(_) => path.canonicalize()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = parent.unwrap_or_else(|| Path::new(".")).canonicalize()?;
                parent.join(path.file_name().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "database path has no file name",
                    )
                })?)
            }
            Err(error) => return Err(error.into()),
        };
        let lock_path = database_path.with_extension("lock");
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        lock_file
            .try_lock_exclusive()
            .map_err(|_| JournalError::AlreadyRunning)?;
        let lock = JournalLock { file: lock_file };
        let mut connection = Connection::open(&database_path)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS schema_version(version INTEGER NOT NULL);
             INSERT INTO schema_version(version) SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM schema_version);
             CREATE TABLE IF NOT EXISTS operations(
               controller_id TEXT NOT NULL,
               instance_id TEXT NOT NULL,
               command TEXT NOT NULL,
               state TEXT NOT NULL,
               tombstone INTEGER NOT NULL DEFAULT 0,
               detail TEXT,
               updated_at INTEGER NOT NULL,
               PRIMARY KEY(controller_id, instance_id)
             );
             CREATE TABLE IF NOT EXISTS webhook_deliveries(
               delivery_id TEXT PRIMARY KEY,
               repository_id INTEGER NOT NULL,
               event TEXT NOT NULL,
               headers_json TEXT NOT NULL,
               body BLOB NOT NULL,
               state TEXT NOT NULL,
               detail TEXT,
               created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS ingress_outbox(
               id TEXT PRIMARY KEY,
               kind TEXT NOT NULL,
               destination TEXT NOT NULL,
               headers_json TEXT NOT NULL,
               body BLOB NOT NULL,
               state TEXT NOT NULL,
               attempts INTEGER NOT NULL DEFAULT 0,
               detail TEXT,
               created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS ingress_outbox_pending ON ingress_outbox(state, created_at);
             CREATE TABLE IF NOT EXISTS pool_snapshots(
               id TEXT PRIMARY KEY,
               repository_id INTEGER NOT NULL,
               request_token TEXT NOT NULL,
               config_json TEXT NOT NULL,
               state TEXT NOT NULL,
               garm_pool_id TEXT,
               expires_at INTEGER NOT NULL,
               created_at INTEGER NOT NULL,
               UNIQUE(repository_id, request_token)
             );
             CREATE TABLE IF NOT EXISTS terminal_runs(
               repository_id INTEGER NOT NULL,
               run_id INTEGER NOT NULL,
               run_attempt INTEGER NOT NULL,
               completed_at INTEGER NOT NULL,
               PRIMARY KEY(repository_id, run_id, run_attempt)
             );
             CREATE TABLE IF NOT EXISTS job_observations(
               repository_id INTEGER NOT NULL,
               run_id INTEGER NOT NULL,
               run_attempt INTEGER NOT NULL,
               job_id INTEGER NOT NULL,
               head_sha TEXT NOT NULL,
               status TEXT NOT NULL,
               runner_name TEXT,
               runner_id INTEGER,
               started_at TEXT,
               completed_at TEXT,
               updated_at INTEGER NOT NULL,
               PRIMARY KEY(repository_id, run_id, run_attempt, job_id)
             );
             CREATE TABLE IF NOT EXISTS allocations(
               allocation_id TEXT NOT NULL UNIQUE,
               controller_id TEXT NOT NULL,
               runner_name TEXT NOT NULL,
               snapshot_id TEXT NOT NULL,
               catalog_fingerprint TEXT,
               pool_id TEXT NOT NULL,
               account_id TEXT,
               state TEXT NOT NULL,
               selected_offer_json TEXT,
               binding_json TEXT,
               provider_instance_json TEXT,
               native_identity_json TEXT,
               native_identity_key TEXT,
               detail TEXT,
               created_at INTEGER NOT NULL,
               dispatched_at INTEGER,
               returned_at INTEGER,
               tombstone INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY(controller_id, runner_name)
             );
             CREATE INDEX IF NOT EXISTS allocations_pool_state ON allocations(controller_id, pool_id, state);
             CREATE TABLE IF NOT EXISTS child_attempts(
               allocation_id TEXT NOT NULL REFERENCES allocations(allocation_id),
               attempt_number INTEGER NOT NULL,
               state TEXT NOT NULL,
               offer_json TEXT NOT NULL,
               binding_json TEXT NOT NULL,
               detail TEXT,
               created_at INTEGER NOT NULL,
               dispatched_at INTEGER,
               completed_at INTEGER,
               PRIMARY KEY(allocation_id, attempt_number)
             );
             CREATE TABLE IF NOT EXISTS allocation_actions(
               action_id INTEGER PRIMARY KEY AUTOINCREMENT,
               allocation_id TEXT NOT NULL REFERENCES allocations(allocation_id),
               command TEXT NOT NULL,
               state TEXT NOT NULL,
               started_at INTEGER NOT NULL,
               completed_at INTEGER,
               detail TEXT,
               unresolved_create INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS allocation_actions_pending ON allocation_actions(allocation_id, action_id) WHERE state IN ('pending','intent','ambiguous');",
        )?;
        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 1 {
            connection.execute("UPDATE schema_version SET version=2", [])?;
        }
        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 2 {
            let transaction = connection.transaction()?;
            transaction.execute_batch(
                "ALTER TABLE webhook_deliveries ADD COLUMN retry_order INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE ingress_outbox ADD COLUMN retry_order INTEGER NOT NULL DEFAULT 0;
                 CREATE INDEX IF NOT EXISTS webhook_deliveries_pending ON webhook_deliveries(state, retry_order, created_at);
                 CREATE INDEX IF NOT EXISTS ingress_outbox_fair ON ingress_outbox(state, retry_order, created_at);
                 UPDATE schema_version SET version=3;",
            )?;
            transaction.commit()?;
        }

        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 3 {
            connection.execute("UPDATE schema_version SET version=4", [])?;
        }

        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 4 {
            let has_account_scope: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('allocations') WHERE name='account_id')",
                [],
                |row| row.get(0),
            )?;
            if !has_account_scope {
                connection.execute("ALTER TABLE allocations ADD COLUMN account_id TEXT", [])?;
            }
            connection.execute("UPDATE schema_version SET version=5", [])?;
        }

        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 5 {
            let has_catalog_fingerprint: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('allocations') WHERE name='catalog_fingerprint')",
                [],
                |row| row.get(0),
            )?;
            if !has_catalog_fingerprint {
                connection.execute(
                    "ALTER TABLE allocations ADD COLUMN catalog_fingerprint TEXT",
                    [],
                )?;
            }
            connection.execute("UPDATE schema_version SET version=6", [])?;
        }

        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 6 {
            let has_native_identity_key: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('allocations') WHERE name='native_identity_key')",
                [],
                |row| row.get(0),
            )?;
            if !has_native_identity_key {
                connection.execute(
                    "ALTER TABLE allocations ADD COLUMN native_identity_key TEXT",
                    [],
                )?;
            }
            let mut statement = connection.prepare(
                "SELECT allocation_id, native_identity_json FROM allocations WHERE native_identity_json IS NOT NULL",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let persisted = rows.collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            for (allocation_id, identity_json) in persisted {
                let identity: crate::cloud::PlacementIdentity =
                    serde_json::from_str(&identity_json)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;
                connection.execute(
                    "UPDATE allocations SET native_identity_key=?2 WHERE allocation_id=?1",
                    params![allocation_id, native_identity_key(&identity)],
                )?;
            }
            connection.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS allocations_native_identity ON allocations(native_identity_key) WHERE native_identity_key IS NOT NULL;
                 UPDATE schema_version SET version=7;",
            )?;
        }

        Self::migrate_interruption_events(&connection)?;
        Self::migrate_candidate_history(&mut connection)?;
        Self::migrate_rerun_intents(&connection)?;
        Self::migrate_allocation_delete_fences(&mut connection)?;
        Self::migrate_cleanup_retries(&mut connection)?;
        Self::migrate_cleanup_resolutions(&mut connection)?;
        Self::migrate_retention(&mut connection)?;
        Self::migrate_outbox_retries(&mut connection)?;

        Ok(Self {
            connection: Mutex::new(connection),
            _lock: lock,
        })
    }

    fn migrate_interruption_events(connection: &Connection) -> Result<(), JournalError> {
        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 7 {
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS interruption_events(
                   allocation_id TEXT NOT NULL REFERENCES allocations(allocation_id),
                   controller_id TEXT NOT NULL,
                   pool_id TEXT NOT NULL,
                   catalog_fingerprint TEXT NOT NULL,
                   configuration_fingerprint TEXT NOT NULL,
                   provider_pin_json TEXT NOT NULL,
                   identity_json TEXT NOT NULL,
                   cloud TEXT NOT NULL,
                   account_id TEXT NOT NULL,
                   location TEXT NOT NULL,
                   resource_id TEXT NOT NULL,
                   source_api TEXT NOT NULL,
                   api_version TEXT NOT NULL,
                   source_id TEXT NOT NULL,
                   event_id TEXT NOT NULL,
                   occurred_at TEXT NOT NULL,
                   observed_at TEXT NOT NULL,
                   reason_fields_json TEXT NOT NULL,
                   PRIMARY KEY(cloud, account_id, location, resource_id, event_id)
                 );
                 CREATE INDEX IF NOT EXISTS interruption_events_allocation ON interruption_events(allocation_id, occurred_at);
                 UPDATE schema_version SET version=8;",
            )?;
        } else if !matches!(version, 8..=16) {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        Ok(())
    }

    fn migrate_candidate_history(connection: &mut Connection) -> Result<(), JournalError> {
        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 8 {
            let transaction = connection.transaction()?;
            transaction.execute_batch(
                "CREATE TABLE create_history(
                   allocation_id TEXT PRIMARY KEY REFERENCES allocations(allocation_id),
                   candidate_key TEXT NOT NULL,
                   catalog_fingerprint TEXT NOT NULL,
                   provider_pin_json TEXT NOT NULL,
                   observed_at_millis INTEGER NOT NULL,
                   latency_nanos INTEGER
                 );
                 CREATE INDEX create_history_candidate ON create_history(candidate_key, observed_at_millis);
                 CREATE TABLE running_history(
                   allocation_id TEXT NOT NULL REFERENCES allocations(allocation_id),
                   candidate_key TEXT NOT NULL,
                   catalog_fingerprint TEXT NOT NULL,
                   provider_pin_json TEXT NOT NULL,
                   identity_json TEXT NOT NULL,
                   session_id TEXT NOT NULL,
                   observed_at_millis INTEGER NOT NULL,
                   running INTEGER NOT NULL,
                   PRIMARY KEY(allocation_id, session_id, observed_at_millis)
                 );
                 CREATE INDEX running_history_candidate ON running_history(candidate_key, observed_at_millis);
                 CREATE TABLE interruption_history(
                   allocation_id TEXT PRIMARY KEY REFERENCES allocations(allocation_id),
                   candidate_key TEXT NOT NULL,
                   catalog_fingerprint TEXT NOT NULL,
                   provider_pin_json TEXT NOT NULL,
                   occurred_at_millis INTEGER NOT NULL
                 );
                 CREATE INDEX interruption_history_candidate ON interruption_history(candidate_key, occurred_at_millis);
                 UPDATE schema_version SET version=9;",
            )?;
            transaction.commit()?;
        } else if !matches!(version, 9..=16) {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        Ok(())
    }

    fn migrate_rerun_intents(connection: &Connection) -> Result<(), JournalError> {
        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 9 {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(
                "ALTER TABLE job_observations ADD COLUMN conclusion TEXT;
                 ALTER TABLE job_observations ADD COLUMN run_status TEXT NOT NULL DEFAULT 'unknown';
                 ALTER TABLE job_observations ADD COLUMN run_conclusion TEXT;
                 ALTER TABLE job_observations ADD COLUMN repository_owner TEXT NOT NULL DEFAULT '';
                 ALTER TABLE job_observations ADD COLUMN repository_name TEXT NOT NULL DEFAULT '';
                 CREATE TABLE rerun_intents(
                   repository_id INTEGER NOT NULL,
                   owner TEXT NOT NULL,
                   repository TEXT NOT NULL,
                   github_api_origin TEXT NOT NULL DEFAULT '',
                   run_id INTEGER NOT NULL,
                   run_attempt INTEGER NOT NULL,
                   job_id INTEGER NOT NULL,
                   state TEXT NOT NULL,
                   created_at INTEGER NOT NULL,
                   updated_at INTEGER NOT NULL,
                   PRIMARY KEY(repository_id, run_id, run_attempt, job_id)
                 );
                 CREATE INDEX rerun_intents_run ON rerun_intents(repository_id, run_id);
                 CREATE TABLE rerun_scan_cursors(name TEXT PRIMARY KEY, offset INTEGER NOT NULL);
                 UPDATE schema_version SET version=10;",
            )?;
            transaction.commit()?;
        } else if !matches!(version, 10..=16) {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 10 {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(
                "ALTER TABLE terminal_runs ADD COLUMN cancelled INTEGER NOT NULL DEFAULT 0 CHECK(cancelled IN (0, 1));
                 UPDATE schema_version SET version=11;",
            )?;
            transaction.commit()?;
        } else if !matches!(version, 11..=16) {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        Ok(())
    }

    fn migrate_allocation_delete_fences(connection: &mut Connection) -> Result<(), JournalError> {
        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 11 {
            let transaction = connection.transaction()?;
            transaction.execute_batch(
                "CREATE TABLE allocation_delete_fences(
                   controller_id TEXT NOT NULL,
                   identity_key TEXT NOT NULL,
                   created_at INTEGER NOT NULL,
                   PRIMARY KEY(controller_id, identity_key)
                 );
                 UPDATE schema_version SET version=12;",
            )?;
            transaction.commit()?;
        } else if !matches!(version, 12..=16) {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        Ok(())
    }

    fn migrate_cleanup_retries(connection: &mut Connection) -> Result<(), JournalError> {
        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 12 {
            let transaction = connection.transaction()?;
            transaction.execute_batch(
                "ALTER TABLE allocation_actions ADD COLUMN reusable INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE allocation_actions ADD COLUMN attempt_count INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE allocation_actions ADD COLUMN last_attempt_at INTEGER;
                 ALTER TABLE allocation_actions ADD COLUMN next_attempt_at INTEGER;
                 UPDATE allocation_actions SET reusable=1 WHERE command='DeleteInstance' AND state IN ('pending','intent','ambiguous') AND action_id=(SELECT MIN(candidate.action_id) FROM allocation_actions candidate WHERE candidate.allocation_id=allocation_actions.allocation_id AND candidate.command='DeleteInstance' AND candidate.state IN ('pending','intent','ambiguous'));
                 UPDATE allocation_actions SET attempt_count=(SELECT COUNT(*) FROM allocation_actions history WHERE history.allocation_id=allocation_actions.allocation_id AND history.command='DeleteInstance' AND history.state IN ('ambiguous','complete','intent')) WHERE reusable=1;
                 UPDATE allocation_actions SET last_attempt_at=(SELECT MAX(history.started_at) FROM allocation_actions history WHERE history.allocation_id=allocation_actions.allocation_id AND history.command='DeleteInstance' AND history.state IN ('ambiguous','complete','intent')) WHERE reusable=1 AND attempt_count>0;
                 UPDATE allocation_actions SET state='pending', next_attempt_at=CAST(strftime('%s','now') AS INTEGER)+CASE WHEN attempt_count>0 THEN 1 ELSE 0 END WHERE reusable=1 AND state IN ('pending','intent','ambiguous');
                 CREATE UNIQUE INDEX allocation_actions_reusable_delete ON allocation_actions(allocation_id) WHERE reusable=1;
                 UPDATE schema_version SET version=13;",
            )?;
            transaction.commit()?;
        } else if !matches!(version, 13..=16) {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        Ok(())
    }

    fn migrate_cleanup_resolutions(connection: &mut Connection) -> Result<(), JournalError> {
        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 13 {
            let transaction = connection.transaction()?;
            transaction.execute_batch(
                "CREATE TABLE cleanup_resolution_audit(
                   allocation_id TEXT PRIMARY KEY REFERENCES allocations(allocation_id),
                   attestation_digest TEXT NOT NULL UNIQUE,
                   evidence_reference TEXT NOT NULL,
                   verified_at INTEGER NOT NULL,
                   committed_at INTEGER NOT NULL,
                   uid INTEGER NOT NULL,
                   gid INTEGER NOT NULL,
                   summary_json TEXT NOT NULL
                 );
                 CREATE TRIGGER cleanup_resolution_audit_no_update
                   BEFORE UPDATE ON cleanup_resolution_audit
                   BEGIN SELECT RAISE(ABORT, 'cleanup resolution audit is append-only'); END;
                 CREATE TRIGGER cleanup_resolution_audit_no_delete
                   BEFORE DELETE ON cleanup_resolution_audit
                   BEGIN SELECT RAISE(ABORT, 'cleanup resolution audit is append-only'); END;
                 UPDATE schema_version SET version=14;",
            )?;
            transaction.commit()?;
        } else if !matches!(version, 14..=16) {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        Ok(())
    }

    fn migrate_outbox_retries(connection: &mut Connection) -> Result<(), JournalError> {
        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 15 {
            let transaction = connection.transaction()?;
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('ingress_outbox') WHERE name='next_attempt_at')",
                [],
                |row| row.get(0),
            )?;
            if !exists {
                transaction.execute(
                    "ALTER TABLE ingress_outbox ADD COLUMN next_attempt_at INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
            }

            transaction.execute_batch(
                "CREATE INDEX IF NOT EXISTS ingress_outbox_due ON ingress_outbox(state, next_attempt_at, retry_order);
                 UPDATE schema_version SET version=16;",
            )?;
            transaction.commit()?;
        }

        Ok(())
    }

    fn migrate_retention(connection: &mut Connection) -> Result<(), JournalError> {
        let version: u32 =
            connection.query_row("SELECT version FROM schema_version", [], |row| row.get(0))?;
        if version == 14 {
            let transaction = connection.transaction()?;
            transaction.execute_batch(
                "CREATE TABLE IF NOT EXISTS history_retention_floors(
                   candidate_key TEXT NOT NULL,
                   catalog_fingerprint TEXT NOT NULL,
                   provider_pin_json TEXT NOT NULL,
                   dimension TEXT NOT NULL,
                   floor_millis INTEGER NOT NULL,
                   PRIMARY KEY(candidate_key, catalog_fingerprint, provider_pin_json, dimension)
                 );
                 UPDATE schema_version SET version=15;",
            )?;
            for (table, column, definition) in [
                ("webhook_deliveries", "payload_sha256", "TEXT"),
                ("webhook_deliveries", "terminal_at", "INTEGER"),
                ("ingress_outbox", "payload_sha256", "TEXT"),
                ("ingress_outbox", "terminal_at", "INTEGER"),
                ("pool_snapshots", "config_sha256", "TEXT"),
                ("pool_snapshots", "compacted_at", "INTEGER"),
                ("pool_snapshots", "terminal_at", "INTEGER"),
            ] {
                let exists: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2)",
                    params![table, column],
                    |row| row.get(0),
                )?;
                if !exists {
                    transaction.execute_batch(&format!(
                        "ALTER TABLE {table} ADD COLUMN {column} {definition};"
                    ))?;
                }
            }

            let now = Self::now() as i64;
            let mut after_delivery_id: Option<String> = None;
            loop {
                let query = if after_delivery_id.is_some() {
                    "SELECT delivery_id, repository_id, event, headers_json, body, state FROM webhook_deliveries WHERE delivery_id>?1 AND payload_sha256 IS NULL ORDER BY delivery_id LIMIT 32"
                } else {
                    "SELECT delivery_id, repository_id, event, headers_json, body, state FROM webhook_deliveries WHERE ?1 IS NULL AND payload_sha256 IS NULL ORDER BY delivery_id LIMIT 32"
                };
                let mut statement = transaction.prepare(query)?;
                let rows = statement.query_map([after_delivery_id.as_deref()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?;
                let deliveries = rows.collect::<Result<Vec<_>, _>>()?;
                drop(statement);
                if deliveries.is_empty() {
                    break;
                }

                after_delivery_id = deliveries.last().map(|delivery| delivery.0.clone());
                for (id, repository_id, event, headers, body, state) in deliveries {
                    let digest = Self::delivery_digest(&id, repository_id, &event, &headers, &body);
                    let terminal_at =
                        matches!(state.as_str(), "queued" | "rejected").then_some(now);
                    transaction.execute("UPDATE webhook_deliveries SET payload_sha256=?2, terminal_at=?3 WHERE delivery_id=?1", params![id, digest, terminal_at])?;
                }
            }

            let mut after_outbox_id: Option<String> = None;
            loop {
                let query = if after_outbox_id.is_some() {
                    "SELECT id, kind, destination, headers_json, body, state FROM ingress_outbox WHERE id>?1 AND payload_sha256 IS NULL ORDER BY id LIMIT 32"
                } else {
                    "SELECT id, kind, destination, headers_json, body, state FROM ingress_outbox WHERE ?1 IS NULL AND payload_sha256 IS NULL ORDER BY id LIMIT 32"
                };
                let mut statement = transaction.prepare(query)?;
                let rows = statement.query_map([after_outbox_id.as_deref()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?;
                let messages = rows.collect::<Result<Vec<_>, _>>()?;
                drop(statement);
                if messages.is_empty() {
                    break;
                }

                after_outbox_id = messages.last().map(|message| message.0.clone());
                for (id, kind, destination, headers, body, state) in messages {
                    let digest = Self::outbox_digest(&id, &kind, &destination, &headers, &body);
                    let terminal_at =
                        matches!(state.as_str(), "delivered" | "rejected").then_some(now);
                    transaction.execute(
                        "UPDATE ingress_outbox SET payload_sha256=?2, terminal_at=?3 WHERE id=?1",
                        params![id, digest, terminal_at],
                    )?;
                }
            }

            let mut after_snapshot_id: Option<String> = None;
            loop {
                let query = if after_snapshot_id.is_some() {
                    "SELECT id, config_json FROM pool_snapshots WHERE id>?1 AND config_sha256 IS NULL ORDER BY id LIMIT 32"
                } else {
                    "SELECT id, config_json FROM pool_snapshots WHERE ?1 IS NULL AND config_sha256 IS NULL ORDER BY id LIMIT 32"
                };
                let mut statement = transaction.prepare(query)?;
                let rows = statement.query_map([after_snapshot_id.as_deref()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                let snapshots = rows.collect::<Result<Vec<_>, _>>()?;
                drop(statement);
                if snapshots.is_empty() {
                    break;
                }

                after_snapshot_id = snapshots.last().map(|snapshot| snapshot.0.clone());
                for (id, config_json) in snapshots {
                    transaction.execute(
                        "UPDATE pool_snapshots SET config_sha256=?2, terminal_at=CASE WHEN state IN ('deleted','rejected') THEN ?3 ELSE NULL END WHERE id=?1",
                        params![id, Self::sha256(config_json.as_bytes()), now],
                    )?;
                }
            }

            transaction.commit()?;
        }
        Ok(())
    }
}
