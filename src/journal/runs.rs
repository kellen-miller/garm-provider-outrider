use super::{
    CompletedFailedJob, JobObservationRecord, JobRerunReservation, Journal, JournalError,
    RerunIntent,
};
use rusqlite::{OptionalExtension, params};

impl Journal {
    /// Record a signed workflow-run completion so late queued events cannot recreate pools.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn mark_run_terminal(
        &self,
        repository_id: u64,
        run_id: u64,
        attempt: u32,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute("INSERT INTO terminal_runs(repository_id, run_id, run_attempt, completed_at, cancelled) VALUES (?1, ?2, ?3, ?4, 0) ON CONFLICT(repository_id, run_id, run_attempt) DO NOTHING", params![i64::try_from(repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::try_from(run_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::from(attempt), Self::now() as i64])?;
        Ok(())
    }

    /// Persist an explicit cancellation veto for one workflow attempt.
    ///
    /// # Errors
    /// Returns `JournalError` when storage access or identity conversion fails.
    pub fn mark_run_cancelled(
        &self,
        repository_id: u64,
        run_id: u64,
        attempt: u32,
    ) -> Result<(), JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute(
            "INSERT INTO terminal_runs(repository_id, run_id, run_attempt, completed_at, cancelled) VALUES (?1, ?2, ?3, ?4, 1) ON CONFLICT(repository_id, run_id, run_attempt) DO UPDATE SET cancelled=1",
            params![i64::try_from(repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::try_from(run_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::from(attempt), Self::now() as i64],
        )?;
        Ok(())
    }

    /// Check whether a particular run attempt has a durable completion event.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn run_is_terminal(
        &self,
        repository_id: u64,
        run_id: u64,
        attempt: u32,
    ) -> Result<bool, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM terminal_runs WHERE repository_id=?1 AND run_id=?2 AND run_attempt=?3)", params![i64::try_from(repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::try_from(run_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::from(attempt)], |row| row.get(0))?)
    }

    /// Check whether a workflow attempt has a durable cancellation veto.
    ///
    /// # Errors
    /// Returns `JournalError` when storage access or identity conversion fails.
    pub fn run_is_cancelled(
        &self,
        repository_id: u64,
        run_id: u64,
        attempt: u32,
    ) -> Result<bool, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM terminal_runs WHERE repository_id=?1 AND run_id=?2 AND run_attempt=?3 AND cancelled=1)",
            params![i64::try_from(repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::try_from(run_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::from(attempt)],
            |row| row.get(0),
        )?)
    }

    /// Persist authenticated runner assignment and terminal observations by exact GitHub job identity.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn record_job_observation(
        &self,
        observation: &JobObservationRecord,
    ) -> Result<(), JournalError> {
        let mut normalized = observation.clone();
        normalized.runner_name = normalized
            .runner_name
            .filter(|name| !name.trim().is_empty());
        normalized.runner_id = normalized.runner_id.filter(|runner_id| *runner_id > 0);
        normalized.started_at = normalized
            .started_at
            .filter(|timestamp| !timestamp.trim().is_empty());
        normalized.completed_at = normalized
            .completed_at
            .filter(|timestamp| !timestamp.trim().is_empty());
        let observation = &normalized;
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        let observation_runner_id = observation
            .runner_id
            .map(i64::try_from)
            .transpose()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let key = params![
            i64::try_from(observation.repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?,
            i64::try_from(observation.run_id).map_err(|_| rusqlite::Error::InvalidQuery)?,
            i64::from(observation.run_attempt),
            i64::try_from(observation.job_id).map_err(|_| rusqlite::Error::InvalidQuery)?,
        ];
        tx.execute(
            "UPDATE job_observations SET runner_name=CASE WHEN trim(COALESCE(runner_name, ''))='' THEN NULL ELSE runner_name END, runner_id=CASE WHEN runner_id=0 THEN NULL ELSE runner_id END, started_at=CASE WHEN trim(COALESCE(started_at, ''))='' THEN NULL ELSE started_at END, completed_at=CASE WHEN trim(COALESCE(completed_at, ''))='' THEN NULL ELSE completed_at END WHERE repository_id=?1 AND run_id=?2 AND run_attempt=?3 AND job_id=?4",
            params![
                i64::try_from(observation.repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?,
                i64::try_from(observation.run_id).map_err(|_| rusqlite::Error::InvalidQuery)?,
                i64::from(observation.run_attempt),
                i64::try_from(observation.job_id).map_err(|_| rusqlite::Error::InvalidQuery)?,
            ],
        )?;
        let existing = tx
            .query_row(
                "SELECT status, head_sha, conclusion, runner_name, runner_id, started_at, completed_at, run_status, run_conclusion, repository_owner, repository_name FROM job_observations WHERE repository_id=?1 AND run_id=?2 AND run_attempt=?3 AND job_id=?4",
                key,
                |row| Ok((
                    row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?, row.get::<_, Option<i64>>(4)?, row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?, row.get::<_, String>(7)?, row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?, row.get::<_, String>(10)?,
                )),
            )
            .optional()?;
        if let Some(existing) = existing.as_ref()
            && existing.0 == "completed"
            && observation.status == "completed"
            && (existing.1 != observation.head_sha
                || existing
                    .2
                    .as_ref()
                    .zip(observation.conclusion.as_ref())
                    .is_some_and(|(old, new)| old != new)
                || existing
                    .3
                    .as_ref()
                    .zip(observation.runner_name.as_ref())
                    .is_some_and(|(old, new)| old != new)
                || existing
                    .4
                    .zip(observation_runner_id)
                    .is_some_and(|(old, new)| old != new)
                || existing
                    .5
                    .as_ref()
                    .zip(observation.started_at.as_ref())
                    .is_some_and(|(old, new)| old != new)
                || existing
                    .6
                    .as_ref()
                    .zip(observation.completed_at.as_ref())
                    .is_some_and(|(old, new)| old != new)
                || (existing.7 == "completed"
                    && existing
                        .8
                        .as_ref()
                        .zip(observation.run_conclusion.as_ref())
                        .is_some_and(|(old, new)| old != new))
                || (!existing.9.is_empty()
                    && !observation.repository_owner.is_empty()
                    && existing.9 != observation.repository_owner)
                || (!existing.10.is_empty()
                    && !observation.repository_name.is_empty()
                    && existing.10 != observation.repository_name))
        {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        tx.execute("INSERT INTO job_observations(repository_id, run_id, run_attempt, job_id, head_sha, status, runner_name, runner_id, started_at, completed_at, updated_at, conclusion, run_status, run_conclusion, repository_owner, repository_name) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16) ON CONFLICT(repository_id, run_id, run_attempt, job_id) DO UPDATE SET head_sha=CASE WHEN job_observations.status='completed' THEN job_observations.head_sha ELSE excluded.head_sha END, status=CASE WHEN job_observations.status='completed' THEN 'completed' WHEN job_observations.status='in_progress' AND excluded.status IN ('queued','waiting') THEN 'in_progress' ELSE excluded.status END, runner_name=COALESCE(job_observations.runner_name, excluded.runner_name), runner_id=COALESCE(job_observations.runner_id, excluded.runner_id), started_at=COALESCE(job_observations.started_at, excluded.started_at), completed_at=COALESCE(job_observations.completed_at, excluded.completed_at), conclusion=CASE WHEN excluded.status='completed' THEN COALESCE(job_observations.conclusion, excluded.conclusion) ELSE job_observations.conclusion END, run_status=CASE WHEN job_observations.run_status='completed' THEN 'completed' WHEN job_observations.run_status='in_progress' AND excluded.run_status IN ('queued','waiting','requested','pending') THEN 'in_progress' ELSE excluded.run_status END, run_conclusion=CASE WHEN excluded.run_status='completed' THEN COALESCE(job_observations.run_conclusion, excluded.run_conclusion) ELSE job_observations.run_conclusion END, repository_owner=CASE WHEN job_observations.repository_owner='' THEN excluded.repository_owner ELSE job_observations.repository_owner END, repository_name=CASE WHEN job_observations.repository_name='' THEN excluded.repository_name ELSE job_observations.repository_name END, updated_at=excluded.updated_at", params![i64::try_from(observation.repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::try_from(observation.run_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::from(observation.run_attempt), i64::try_from(observation.job_id).map_err(|_| rusqlite::Error::InvalidQuery)?, observation.head_sha, observation.status, observation.runner_name, observation_runner_id, observation.started_at, observation.completed_at, Self::now() as i64, observation.conclusion, observation.run_status, observation.run_conclusion, observation.repository_owner, observation.repository_name])?;
        tx.commit()?;
        Ok(())
    }

    /// Read the last authenticated assignment for one exact workflow job.
    ///
    /// # Errors
    /// Returns `JournalError` when SQLite operations fail or stored values are invalid.
    pub fn job_observation(
        &self,
        repository_id: u64,
        run_id: u64,
        attempt: u32,
        job_id: u64,
    ) -> Result<Option<JobObservationRecord>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(connection.query_row("SELECT repository_id, run_id, run_attempt, job_id, head_sha, status, runner_name, runner_id, started_at, completed_at, conclusion, run_status, run_conclusion, repository_owner, repository_name FROM job_observations WHERE repository_id=?1 AND run_id=?2 AND run_attempt=?3 AND job_id=?4", params![i64::try_from(repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::try_from(run_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::from(attempt), i64::try_from(job_id).map_err(|_| rusqlite::Error::InvalidQuery)?], |row| Ok(JobObservationRecord {
            repository_id: u64::try_from(row.get::<_, i64>(0)?).map_err(|_| rusqlite::Error::InvalidQuery)?, run_id: u64::try_from(row.get::<_, i64>(1)?).map_err(|_| rusqlite::Error::InvalidQuery)?, run_attempt: u32::try_from(row.get::<_, i64>(2)?).map_err(|_| rusqlite::Error::InvalidQuery)?, job_id: u64::try_from(row.get::<_, i64>(3)?).map_err(|_| rusqlite::Error::InvalidQuery)?, head_sha: row.get(4)?, status: row.get(5)?, runner_name: row.get(6)?, runner_id: row.get::<_, Option<i64>>(7)?.map(u64::try_from).transpose().map_err(|_| rusqlite::Error::InvalidQuery)?, started_at: row.get(8)?, completed_at: row.get(9)?,
            conclusion: row.get(10)?, run_status: row.get(11)?, run_conclusion: row.get(12)?, repository_owner: row.get(13)?, repository_name: row.get(14)?,
        })).optional()?)
    }

    /// Read a bounded batch of authenticated failed job observations for reconciliation.
    ///
    /// # Errors
    /// Returns `JournalError` when storage access or identity conversion fails.
    pub fn completed_failed_jobs(
        &self,
        limit: usize,
    ) -> Result<Vec<CompletedFailedJob>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        let total: i64 = tx.query_row(
            "SELECT COUNT(*) FROM job_observations WHERE status='completed' AND conclusion='failure' AND repository_owner!='' AND repository_name!='' AND runner_name IS NOT NULL AND runner_id IS NOT NULL AND started_at IS NOT NULL AND completed_at IS NOT NULL AND NOT EXISTS(SELECT 1 FROM rerun_intents WHERE rerun_intents.repository_id=job_observations.repository_id AND rerun_intents.run_id=job_observations.run_id AND rerun_intents.run_attempt=job_observations.run_attempt AND rerun_intents.job_id=job_observations.job_id)",
            [],
            |row| row.get(0),
        )?;
        let (offset, first_limit, wrap_limit) =
            Self::rerun_scan_window(&tx, "failed_jobs", total, limit)?;
        let mut candidates = Vec::new();
        let sql = "SELECT repository_id, repository_owner, repository_name, run_id, run_attempt, job_id, head_sha, runner_name, runner_id, started_at, completed_at FROM job_observations WHERE status='completed' AND conclusion='failure' AND repository_owner!='' AND repository_name!='' AND runner_name IS NOT NULL AND runner_id IS NOT NULL AND started_at IS NOT NULL AND completed_at IS NOT NULL AND NOT EXISTS(SELECT 1 FROM rerun_intents WHERE rerun_intents.repository_id=job_observations.repository_id AND rerun_intents.run_id=job_observations.run_id AND rerun_intents.run_attempt=job_observations.run_attempt AND rerun_intents.job_id=job_observations.job_id) ORDER BY repository_id, run_id, run_attempt, job_id LIMIT ?1 OFFSET ?2";
        let mut statement = tx.prepare(sql)?;
        let rows = statement.query_map([first_limit, offset], |row| {
            Ok(CompletedFailedJob {
                repository_id: u64::try_from(row.get::<_, i64>(0)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                repository_owner: row.get(1)?,
                repository_name: row.get(2)?,
                run_id: u64::try_from(row.get::<_, i64>(3)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                run_attempt: u32::try_from(row.get::<_, i64>(4)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                job_id: u64::try_from(row.get::<_, i64>(5)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                head_sha: row.get(6)?,
                runner_name: row.get(7)?,
                runner_id: row
                    .get::<_, Option<i64>>(8)?
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                started_at: row.get(9)?,
                completed_at: row.get(10)?,
            })
        })?;
        candidates.extend(rows.collect::<Result<Vec<_>, _>>()?);
        drop(statement);
        if wrap_limit > 0 {
            let mut statement = tx.prepare(sql)?;
            let rows = statement.query_map([wrap_limit, 0_i64], |row| {
                Ok(CompletedFailedJob {
                    repository_id: u64::try_from(row.get::<_, i64>(0)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    repository_owner: row.get(1)?,
                    repository_name: row.get(2)?,
                    run_id: u64::try_from(row.get::<_, i64>(3)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    run_attempt: u32::try_from(row.get::<_, i64>(4)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    job_id: u64::try_from(row.get::<_, i64>(5)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    head_sha: row.get(6)?,
                    runner_name: row.get(7)?,
                    runner_id: row
                        .get::<_, Option<i64>>(8)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    started_at: row.get(9)?,
                    completed_at: row.get(10)?,
                })
            })?;
            candidates.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        tx.commit()?;
        Ok(candidates)
    }

    fn rerun_scan_window(
        tx: &rusqlite::Transaction<'_>,
        name: &str,
        total: i64,
        limit: usize,
    ) -> Result<(i64, i64, i64), JournalError> {
        if total <= 0 || limit == 0 {
            return Ok((0, 0, 0));
        }
        let total = usize::try_from(total).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let previous: Option<i64> = tx
            .query_row(
                "SELECT offset FROM rerun_scan_cursors WHERE name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        let offset = usize::try_from(previous.unwrap_or(0))
            .map_err(|_| rusqlite::Error::InvalidQuery)?
            % total;
        let count = limit.min(total);
        let first = count.min(total - offset);
        let wrap = count - first;
        let next = (offset + count) % total;
        tx.execute(
            "INSERT INTO rerun_scan_cursors(name, offset) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET offset=excluded.offset",
            params![name, i64::try_from(next).map_err(|_| rusqlite::Error::InvalidQuery)?],
        )?;
        Ok((
            i64::try_from(offset).map_err(|_| rusqlite::Error::InvalidQuery)?,
            i64::try_from(first).map_err(|_| rusqlite::Error::InvalidQuery)?,
            i64::try_from(wrap).map_err(|_| rusqlite::Error::InvalidQuery)?,
        ))
    }

    /// Reserve a stable per-run rerun slot and exact job-attempt fence before POST.
    ///
    /// # Errors
    /// Returns `JournalError` when storage access or identity conversion fails.
    pub fn reserve_job_rerun(
        &self,
        reservation: &JobRerunReservation,
    ) -> Result<bool, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        let repository_id =
            i64::try_from(reservation.repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let run_id =
            i64::try_from(reservation.run_id).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let job_id =
            i64::try_from(reservation.job_id).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let attempt = i64::from(reservation.run_attempt);
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM rerun_intents WHERE repository_id=?1 AND run_id=?2 AND run_attempt=?3 AND job_id=?4)",
            params![repository_id, run_id, attempt, job_id],
            |row| row.get(0),
        )?;
        let cancelled: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM terminal_runs WHERE repository_id=?1 AND run_id=?2 AND run_attempt=?3 AND cancelled=1)",
            params![repository_id, run_id, attempt],
            |row| row.get(0),
        )?;
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM rerun_intents WHERE repository_id=?1 AND run_id=?2",
            params![repository_id, run_id],
            |row| row.get(0),
        )?;
        if cancelled || existing || count >= i64::from(reservation.max_per_run) {
            tx.commit()?;
            return Ok(false);
        }
        let now = Self::now() as i64;
        tx.execute(
            "INSERT INTO rerun_intents(repository_id, owner, repository, github_api_origin, run_id, run_attempt, job_id, state, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'intent', ?8, ?8)",
            params![repository_id, reservation.owner, reservation.repository, reservation.github_api_origin, run_id, attempt, job_id, now],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Record the result or ambiguity of one previously reserved rerun POST.
    ///
    /// # Errors
    /// Returns `JournalError` when storage access fails.
    pub fn set_job_rerun_state(
        &self,
        repository_id: u64,
        run_id: u64,
        attempt: u32,
        job_id: u64,
        state: &str,
    ) -> Result<(), JournalError> {
        if !matches!(
            state,
            "acknowledged"
                | "denied"
                | "transport_changed_fenced"
                | "cancellation_veto"
                | "later_attempt_observed"
        ) {
            return Err(JournalError::InvalidRerunState);
        }
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        connection.execute(
            "UPDATE rerun_intents SET state=?5, updated_at=?6 WHERE repository_id=?1 AND run_id=?2 AND run_attempt=?3 AND job_id=?4 AND state IN ('intent','acknowledged')",
            params![i64::try_from(repository_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::try_from(run_id).map_err(|_| rusqlite::Error::InvalidQuery)?, i64::from(attempt), i64::try_from(job_id).map_err(|_| rusqlite::Error::InvalidQuery)?, state, Self::now() as i64],
        )?;
        Ok(())
    }

    /// Read outstanding rerun intents for restart-safe authenticated observation.
    ///
    /// # Errors
    /// Returns `JournalError` when storage access or identity conversion fails.
    pub fn pending_job_reruns(&self, limit: usize) -> Result<Vec<RerunIntent>, JournalError> {
        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let tx = connection.unchecked_transaction()?;
        let total: i64 = tx.query_row(
            "SELECT COUNT(*) FROM rerun_intents WHERE state IN ('intent','acknowledged')",
            [],
            |row| row.get(0),
        )?;
        let (offset, first_limit, wrap_limit) =
            Self::rerun_scan_window(&tx, "pending_intents", total, limit)?;
        let mut intents = Vec::new();
        let sql = "SELECT repository_id, owner, repository, github_api_origin, run_id, run_attempt, job_id, state FROM rerun_intents WHERE state IN ('intent','acknowledged') ORDER BY created_at, repository_id, run_id, run_attempt, job_id LIMIT ?1 OFFSET ?2";
        let mut statement = tx.prepare(sql)?;
        let rows = statement.query_map([first_limit, offset], |row| {
            Ok(RerunIntent {
                repository_id: u64::try_from(row.get::<_, i64>(0)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                owner: row.get(1)?,
                repository: row.get(2)?,
                github_api_origin: row.get(3)?,
                run_id: u64::try_from(row.get::<_, i64>(4)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                run_attempt: u32::try_from(row.get::<_, i64>(5)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                job_id: u64::try_from(row.get::<_, i64>(6)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                state: row.get(7)?,
            })
        })?;
        intents.extend(rows.collect::<Result<Vec<_>, _>>()?);
        drop(statement);
        if wrap_limit > 0 {
            let mut statement = tx.prepare(sql)?;
            let rows = statement.query_map([wrap_limit, 0_i64], |row| {
                Ok(RerunIntent {
                    repository_id: u64::try_from(row.get::<_, i64>(0)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    owner: row.get(1)?,
                    repository: row.get(2)?,
                    github_api_origin: row.get(3)?,
                    run_id: u64::try_from(row.get::<_, i64>(4)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    run_attempt: u32::try_from(row.get::<_, i64>(5)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    job_id: u64::try_from(row.get::<_, i64>(6)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    state: row.get(7)?,
                })
            })?;
            intents.extend(rows.collect::<Result<Vec<_>, _>>()?);
        }
        tx.commit()?;
        Ok(intents)
    }
}
