use super::*;
use serde_json::json;

impl IngressService {
    pub(super) async fn process_delivery(&self, delivery: &WebhookDelivery) -> Result<(), bool> {
        let headers: ForwardHeaders =
            serde_json::from_str(&delivery.headers_json).map_err(|_| false)?;
        if delivery.event != "workflow_job" {
            return self.enqueue_forward(delivery, &headers).map_err(|_| true);
        }
        let event: WorkflowJobEvent = serde_json::from_slice(&delivery.body).map_err(|_| false)?;
        if event.repository.id != delivery.repository_id {
            return Err(false);
        }
        let tagged = event
            .workflow_job
            .labels
            .iter()
            .filter(|label| label.to_ascii_lowercase().starts_with("outrider"))
            .collect::<Vec<_>>();
        if tagged.is_empty() {
            return self.enqueue_forward(delivery, &headers).map_err(|_| true);
        }
        if tagged.len() != 1 || event.workflow_job.labels.len() != 1 {
            self.enqueue_rejection_check(
                &event.repository,
                &event.workflow_job.head_sha,
                event.workflow_job.run_id,
                attempt_or_zero(&event.workflow_job),
                event.workflow_job.id,
                "Outrider jobs require one complete atomic routing label",
            )
            .map_err(|_| true)?;
            return Ok(());
        }
        let token = tagged[0].as_str();
        let request = match RoutingRequest::parse(token) {
            Ok(request) => request,
            Err(_) => {
                self.enqueue_rejection_check(
                    &event.repository,
                    &event.workflow_job.head_sha,
                    event.workflow_job.run_id,
                    attempt_or_zero(&event.workflow_job),
                    event.workflow_job.id,
                    "The Outrider routing label is malformed",
                )
                .map_err(|_| true)?;
                return Ok(());
            }
        };
        let attempt = event.workflow_job.run_attempt.ok_or(false)?;
        if request.repository_id != event.repository.id
            || request.run_id != event.workflow_job.run_id
            || request.run_attempt != attempt
        {
            self.enqueue_rejection_check(
                &event.repository,
                &event.workflow_job.head_sha,
                event.workflow_job.run_id,
                attempt,
                event.workflow_job.id,
                "The routing label does not match this repository, run, or attempt",
            )
            .map_err(|_| true)?;
            return Ok(());
        }
        let policy = self
            .settings
            .repositories
            .get(&event.repository.id.to_string())
            .ok_or(false)?;
        let verified = match self.github.verify_job(&event, policy.allow_forks).await {
            Ok(verified) => verified,
            Err(GitHubError::Request) => return Err(true),
            Err(_) => {
                self.enqueue_rejection_check(
                    &event.repository,
                    &event.workflow_job.head_sha,
                    event.workflow_job.run_id,
                    attempt,
                    event.workflow_job.id,
                    "GitHub run and job provenance could not be verified",
                )
                .map_err(|_| true)?;
                return Ok(());
            }
        };
        self.record_verified_run_barrier(&verified)
            .map_err(|_| true)?;
        self.journal
            .record_job_observation(&JobObservationRecord {
                repository_id: verified.observation.repository_id,
                repository_owner: verified.observation.repository_owner.clone(),
                repository_name: verified.observation.repository_name.clone(),
                run_id: verified.observation.run_id,
                run_attempt: verified.observation.run_attempt,
                job_id: verified.observation.job_id,
                head_sha: verified.observation.head_sha.clone(),
                status: verified.observation.status.clone(),
                conclusion: verified.observation.conclusion.clone(),
                run_status: verified.observation.run_status.clone(),
                run_conclusion: verified.observation.run_conclusion.clone(),
                runner_name: verified.observation.runner_name.clone(),
                runner_id: verified.observation.runner_id,
                started_at: verified.observation.started_at.clone(),
                completed_at: verified.observation.completed_at.clone(),
            })
            .map_err(|_| true)?;
        if event.workflow_job.status != "queued" {
            return self.enqueue_forward(delivery, &headers).map_err(|_| true);
        }
        if verified.job.status != "queued" {
            return Ok(());
        }
        let observed = self
            .journal
            .job_observation(
                verified.base_repository.id,
                verified.run.id,
                verified.run.run_attempt,
                verified.job.id,
            )
            .map_err(|_| true)?;
        if observed.is_some_and(|job| job.status != "queued") {
            return Ok(());
        }
        if self
            .journal
            .run_is_terminal(
                verified.base_repository.id,
                verified.run.id,
                verified.run.run_attempt,
            )
            .map_err(|_| true)?
        {
            self.disable_admission_pool(event.repository.id, token)
                .await?;
            return Ok(());
        }
        let snapshot = match self.prepare_snapshot(&request, &verified).await {
            Ok(snapshot) => snapshot,
            Err(SnapshotFailure::Dependency) => return Err(true),
            Err(SnapshotFailure::Rejected(message)) => {
                self.disable_admission_pool(event.repository.id, token)
                    .await?;
                self.enqueue_rejection_check(
                    &verified.base_repository,
                    &verified.run.head_sha,
                    verified.run.id,
                    verified.run.run_attempt,
                    verified.job.id,
                    message,
                )
                .map_err(|_| true)?;
                return Ok(());
            }
        };
        let document: SnapshotDocument =
            serde_json::from_str(&snapshot.config_json).map_err(|_| false)?;
        let ingress_repository = document
            .central
            .ingress
            .as_ref()
            .and_then(|settings| {
                settings
                    .repositories
                    .get(&document.base_repository_id.to_string())
                    .map(|repository| (settings, repository))
            })
            .ok_or(false)?;
        match self
            .garm
            .ensure_repository_pool(
                &self.journal,
                RepositoryPoolRequest {
                    snapshot: &snapshot,
                },
            )
            .await
        {
            Ok(pool) => {
                let terminal = self
                    .journal
                    .run_is_terminal(
                        verified.base_repository.id,
                        verified.run.id,
                        verified.run.run_attempt,
                    )
                    .map_err(|_| true)?;
                if terminal || snapshot.expires_at <= now() {
                    self.garm
                        .disable_repository_pool(&ingress_repository.1.garm_repository_id, &pool.id)
                        .await
                        .map_err(|_| true)?;
                    self.journal
                        .set_pool_snapshot_state(&snapshot.id, "draining", Some(&pool.id))
                        .map_err(|_| true)?;
                } else {
                    self.enqueue_forward(delivery, &headers).map_err(|_| true)?;
                }
            }
            Err(PoolError::UnsupportedOs) => {
                self.enqueue_rejection_check(
                    &verified.base_repository,
                    &verified.run.head_sha,
                    verified.run.id,
                    verified.run.run_attempt,
                    verified.job.id,
                    "GARM does not support macOS runner pools",
                )
                .map_err(|_| true)?;
            }
            Err(PoolError::Limit) => {
                self.enqueue_rejection_check(
                    &verified.base_repository,
                    &verified.run.head_sha,
                    verified.run.id,
                    verified.run.run_attempt,
                    verified.job.id,
                    "The administrator dynamic pool limit was reached",
                )
                .map_err(|_| true)?;
            }
            Err(_) => return Err(true),
        }
        Ok(())
    }

    pub(super) async fn maybe_rerun_interrupted_job(
        &self,
        verified: &VerifiedJob,
    ) -> Result<bool, bool> {
        let Some(settings) = self.config.reruns.as_ref() else {
            return Ok(false);
        };
        if !settings.enabled
            || !settings.actions_write_permission
            || settings.max_per_run == 0
            || settings.max_per_run > 100
            || verified.job.status != "completed"
            || verified.job.conclusion.as_deref() != Some("failure")
        {
            return Ok(false);
        }
        let (Some(runner_name), Some(runner_id), Some(started_at), Some(completed_at)) = (
            verified.job.runner_name.as_deref(),
            verified.job.runner_id,
            verified.job.started_at.as_deref(),
            verified.job.completed_at.as_deref(),
        ) else {
            return Ok(false);
        };
        if runner_name.is_empty() || runner_id == 0 {
            return Ok(false);
        }
        let started = chrono::DateTime::parse_from_rfc3339(started_at).map_err(|_| false)?;
        let completed = chrono::DateTime::parse_from_rfc3339(completed_at).map_err(|_| false)?;
        if completed <= started {
            return Ok(false);
        }
        let mut matches = self
            .journal
            .allocations_for_controller(&self.settings.controller_id)
            .map_err(|_| true)?
            .into_iter()
            .filter(|allocation| allocation.runner_name == runner_name);
        let Some(allocation) = matches.next() else {
            return Ok(false);
        };
        if matches.next().is_some() {
            return Ok(false);
        }
        let snapshot = self
            .journal
            .pool_snapshot_by_id(&allocation.snapshot_id)
            .map_err(|_| true)?;
        let Some(snapshot) = snapshot else {
            return Ok(false);
        };
        let Ok(document) = serde_json::from_str::<SnapshotDocument>(&snapshot.config_json) else {
            return Ok(false);
        };
        let retained_ingress = document.central.ingress.as_ref();
        let authorized_repository = self
            .settings
            .repositories
            .get(&verified.base_repository.id.to_string());
        if retained_ingress
            .is_none_or(|ingress| ingress.github_api_origin != self.settings.github_api_origin)
        {
            eprintln!(
                "outrider rerun blocked for run {}: retained GitHub API origin changed",
                verified.run.id
            );
            return Err(true);
        }
        if snapshot.repository_id != verified.base_repository.id
            || document.base_repository_id != verified.base_repository.id
            || document.run_id != verified.run.id
            || document.run_attempt != verified.run.run_attempt
            || document.base_repository.owner.login != verified.base_repository.owner.login
            || document.base_repository.name != verified.base_repository.name
            || authorized_repository.is_none()
        {
            return Ok(false);
        }
        let interruptions = self
            .journal
            .interruption_events(&allocation.allocation_id)
            .map_err(|_| true)?;
        let delete_started = self
            .journal
            .first_allocation_delete_intent(&allocation.allocation_id)
            .map_err(|_| true)?;
        let interrupted_during_job = interruptions.into_iter().any(|interruption| {
            let Ok(occurred) = chrono::DateTime::parse_from_rfc3339(&interruption.occurred_at)
            else {
                return false;
            };
            if occurred <= started || occurred >= completed {
                return false;
            }
            !delete_started.is_some_and(|delete_started| {
                i64::try_from(delete_started)
                    .is_ok_and(|delete_started| delete_started <= occurred.timestamp())
            })
        });
        if !interrupted_during_job {
            return Ok(false);
        }
        if !self
            .journal
            .reserve_job_rerun(&JobRerunReservation {
                repository_id: verified.base_repository.id,
                owner: verified.base_repository.owner.login.clone(),
                repository: verified.base_repository.name.clone(),
                github_api_origin: self.settings.github_api_origin.clone(),
                run_id: verified.run.id,
                run_attempt: verified.run.run_attempt,
                job_id: verified.job.id,
                max_per_run: settings.max_per_run,
            })
            .map_err(|_| true)?
        {
            return Ok(false);
        }
        let submission = match self.github.rerun_verified_job(verified).await {
            Ok(submission) => submission,
            Err(_) => {
                eprintln!(
                    "outrider rerun request unresolved for repository {} run {} attempt {} job {}",
                    verified.base_repository.id,
                    verified.run.id,
                    verified.run.run_attempt,
                    verified.job.id
                );
                return Err(true);
            }
        };
        let state = match submission {
            RerunSubmission::Acknowledged => "acknowledged",
            RerunSubmission::Denied => "denied",
        };
        self.journal
            .set_job_rerun_state(
                verified.base_repository.id,
                verified.run.id,
                verified.run.run_attempt,
                verified.job.id,
                state,
            )
            .map_err(|_| true)?;
        Ok(submission == RerunSubmission::Acknowledged)
    }

    pub(super) async fn forward_admission_is_current(
        &self,
        message: &OutboxMessage,
    ) -> Result<bool, bool> {
        let headers: ForwardHeaders =
            serde_json::from_str(&message.headers_json).map_err(|_| false)?;
        if headers.0.get("x-github-event").map(String::as_str) != Some("workflow_job") {
            return Ok(true);
        }
        let event: WorkflowJobEvent = serde_json::from_slice(&message.body).map_err(|_| false)?;
        if event.workflow_job.status != "queued" {
            return Ok(true);
        }
        let tokens = event
            .workflow_job
            .labels
            .iter()
            .filter(|label| label.starts_with("outrider:"))
            .collect::<Vec<_>>();
        if tokens.len() != 1 {
            return Ok(true);
        }
        let request = RoutingRequest::parse(tokens[0]).map_err(|_| false)?;
        let attempt = event.workflow_job.run_attempt.ok_or(false)?;
        let snapshot = self
            .journal
            .pool_snapshot(event.repository.id, tokens[0])
            .map_err(|_| true)?;
        let terminal = self
            .journal
            .run_is_terminal(event.repository.id, request.run_id, attempt)
            .map_err(|_| true)?;
        let expired = snapshot
            .as_ref()
            .is_some_and(|value| value.expires_at <= now());
        let unusable = snapshot.as_ref().is_some_and(|value| {
            matches!(value.state.as_str(), "deleted" | "rejected" | "draining")
        });
        if terminal || expired || unusable {
            self.disable_admission_pool(event.repository.id, tokens[0])
                .await?;
            return Ok(false);
        }
        Ok(true)
    }

    pub(super) async fn prepare_snapshot(
        &self,
        request: &RoutingRequest,
        verified: &VerifiedJob,
    ) -> Result<PoolSnapshot, SnapshotFailure> {
        if let Some(existing) = self
            .journal
            .pool_snapshot(
                verified.base_repository.id,
                &request
                    .format()
                    .map_err(|_| SnapshotFailure::Rejected("The request token is invalid"))?,
            )
            .map_err(|_| SnapshotFailure::Dependency)?
        {
            if !matches!(
                existing.state.as_str(),
                "prepared" | "creating" | "disabled" | "enabled"
            ) {
                return Err(SnapshotFailure::Rejected(
                    "This runner request is terminal or expired",
                ));
            }
            let document: SnapshotDocument = serde_json::from_str(&existing.config_json)
                .map_err(|_| SnapshotFailure::Dependency)?;
            if document.run_id != verified.run.id
                || document.run_attempt != verified.run.run_attempt
                || document.commit_sha != verified.run.head_sha
            {
                return Err(SnapshotFailure::Rejected(
                    "This run request was already bound to different commit provenance",
                ));
            }
            if existing.expires_at <= now() {
                return Err(SnapshotFailure::Rejected(
                    "This runner request is terminal or expired",
                ));
            }
            return Ok(existing);
        }
        let active = self
            .journal
            .active_pool_snapshots()
            .map_err(|_| SnapshotFailure::Dependency)?;
        if active.len() >= self.settings.max_dynamic_pools as usize {
            return Err(SnapshotFailure::Rejected(
                "The administrator dynamic pool limit was reached",
            ));
        }
        let repo_yaml = self
            .github
            .repository_config(&verified.base_repository, &verified.run.head_sha)
            .await
            .map_err(|error| match error {
                GitHubError::Request => SnapshotFailure::Dependency,
                _ => SnapshotFailure::Rejected(
                    "Repository configuration is unavailable at the verified commit",
                ),
            })?;
        let repository = ConfigPatch::from_yaml(&repo_yaml)
            .map_err(|_| SnapshotFailure::Rejected("Repository configuration is invalid"))?;
        let overrides = request
            .job_overrides()
            .map_err(|_| SnapshotFailure::Rejected("The request overrides are invalid"))?;
        let resolved = self
            .config
            .normalize_profile(Some(&repository), &request.profile, Some(&overrides))
            .map_err(|_| {
                SnapshotFailure::Rejected(
                    "The request violates administrator configuration or limits",
                )
            })?;
        let image_catalogs = snapshot_image_catalogs(&self.config, &resolved.profile.image);
        if image_catalogs.is_empty() {
            return Err(SnapshotFailure::Rejected(
                "The profile image alias has no administrator-bound immutable image",
            ));
        }
        let document = SnapshotDocument {
            base_repository: verified.base_repository.clone(),
            base_repository_id: verified.base_repository.id,
            source_repository_id: verified.config_repository.id,
            run_id: verified.run.id,
            run_attempt: verified.run.run_attempt,
            job_id: verified.job.id,
            commit_sha: verified.run.head_sha.clone(),
            central: self.config.clone(),
            repository,
            resolved_profile: resolved.profile,
            image_catalogs,
        };
        let config_json = serde_json::to_string(&document).map_err(|_| {
            SnapshotFailure::Rejected("Resolved configuration could not be snapshotted")
        })?;
        if config_json.len() > 1024 * 1024 {
            return Err(SnapshotFailure::Rejected(
                "Resolved configuration exceeds the snapshot limit",
            ));
        }
        let token = request
            .format()
            .map_err(|_| SnapshotFailure::Rejected("The request token is invalid"))?;
        let mut hash = Sha256::new();
        hash.update(verified.base_repository.id.to_be_bytes());
        hash.update(token.as_bytes());
        let id = hex::encode(hash.finalize());
        let ttl = now()
            .checked_add(self.settings.pool_ttl_seconds)
            .ok_or(SnapshotFailure::Rejected("Pool expiry is out of range"))?;
        let snapshot = PoolSnapshot {
            id,
            repository_id: verified.base_repository.id,
            request_token: token,
            config_json,
            state: "prepared".into(),
            garm_pool_id: None,
            expires_at: ttl,
        };
        self.journal
            .save_pool_snapshot(&snapshot)
            .map_err(|_| SnapshotFailure::Dependency)?;
        Ok(snapshot)
    }

    pub(super) fn enqueue_forward(
        &self,
        delivery: &WebhookDelivery,
        headers: &ForwardHeaders,
    ) -> Result<(), IngressError> {
        let message = OutboxMessage {
            id: format!("forward:{}", delivery.delivery_id),
            kind: "forward".into(),
            destination: self.settings.controller_id.clone(),
            headers_json: serde_json::to_string(headers).map_err(|_| IngressError::Journal)?,
            body: delivery.body.clone(),
            state: "pending".into(),
        };
        self.journal
            .enqueue_outbox(&message)
            .map_err(|_| IngressError::Journal)
    }

    pub(super) fn record_verified_run_barrier(&self, verified: &VerifiedJob) -> Result<(), bool> {
        if verified.run.status != "completed" {
            return Ok(());
        }
        if verified.run.conclusion.as_deref() == Some("cancelled") {
            self.journal
                .mark_run_cancelled(
                    verified.base_repository.id,
                    verified.run.id,
                    verified.run.run_attempt,
                )
                .map_err(|_| true)?;
        } else {
            self.journal
                .mark_run_terminal(
                    verified.base_repository.id,
                    verified.run.id,
                    verified.run.run_attempt,
                )
                .map_err(|_| true)?;
        }
        Ok(())
    }

    pub(super) fn enqueue_rejection_check(
        &self,
        repository: &RepositoryRef,
        sha: &str,
        run_id: u64,
        attempt: u32,
        job_id: u64,
        summary: &str,
    ) -> Result<(), IngressError> {
        if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(());
        }
        let check = json!({
            "name":"Outrider admission", "head_sha":sha,
            "external_id":format!("{}:{}:{}:{}", repository.id, run_id, attempt, job_id),
            "status":"completed", "conclusion":"failure",
            "output":{"title":"Runner request rejected", "summary":summary}
        });
        let id = format!("check:{}:{}:{}:{}", repository.id, run_id, attempt, job_id);
        if self
            .journal
            .outbox_state(&id)
            .map_err(|_| IngressError::Journal)?
            .is_some()
        {
            return Ok(());
        }

        let body = serde_json::to_vec(&CheckOutbox {
            repository: repository.clone(),
            check,
        })
        .map_err(|_| IngressError::Journal)?;
        self.journal
            .enqueue_outbox(&OutboxMessage {
                id,
                kind: "check".into(),
                destination: repository.full_name.clone(),
                headers_json: String::new(),
                body,
                state: "pending".into(),
            })
            .map_err(|_| IngressError::Journal)
    }
}
