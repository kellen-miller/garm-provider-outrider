use super::*;

impl IngressService {
    /// Reauthenticate completed failed jobs and rerun those with causal interruption evidence.
    ///
    /// # Errors
    /// Returns an error when the journal or an authenticated GitHub dependency is unavailable.
    pub async fn rerun_completed_interrupted_jobs(
        &self,
        limit: usize,
    ) -> Result<usize, IngressError> {
        let Some(rerun_settings) = self.config.reruns.as_ref() else {
            return Ok(0);
        };
        if !rerun_settings.enabled || limit == 0 {
            return Ok(0);
        }
        let candidates = self
            .journal
            .completed_failed_jobs(limit)
            .map_err(|_| IngressError::Journal)?;
        let mut rerun_count = 0;
        let mut unresolved = false;
        for candidate in candidates {
            let Some(repository_policy) = self
                .settings
                .repositories
                .get(&candidate.repository_id.to_string())
            else {
                continue;
            };
            let repository = RepositoryRef {
                id: candidate.repository_id,
                name: candidate.repository_name.clone(),
                full_name: format!(
                    "{}/{}",
                    candidate.repository_owner, candidate.repository_name
                ),
                owner: OwnerRef {
                    login: candidate.repository_owner.clone(),
                },
            };
            let event = WorkflowJobEvent {
                action: Some("completed".to_owned()),
                repository: repository.clone(),
                workflow_job: WorkflowJobRef {
                    id: candidate.job_id,
                    run_id: candidate.run_id,
                    run_attempt: Some(candidate.run_attempt),
                    head_sha: candidate.head_sha,
                    status: "completed".to_owned(),
                    conclusion: Some("failure".to_owned()),
                    runner_name: candidate.runner_name,
                    runner_id: candidate.runner_id,
                    started_at: candidate.started_at,
                    completed_at: candidate.completed_at,
                    labels: Vec::new(),
                },
            };
            let verified = match self
                .github
                .verify_job(&event, repository_policy.allow_forks)
                .await
            {
                Ok(verified) => verified,
                Err(GitHubError::Request) => {
                    unresolved = true;
                    continue;
                }
                Err(_) => continue,
            };
            if self
                .journal
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
                .is_err()
            {
                continue;
            }
            let current = match self
                .github
                .current_run(&verified.base_repository, verified.run.id)
                .await
            {
                Ok(current) => current,
                Err(GitHubError::Request) => {
                    unresolved = true;
                    continue;
                }
                Err(_) => continue,
            };
            if current.conclusion.as_deref() == Some("cancelled") {
                self.journal
                    .mark_run_cancelled(
                        verified.base_repository.id,
                        verified.run.id,
                        current.run_attempt,
                    )
                    .map_err(|_| IngressError::Journal)?;
                continue;
            }
            // A newer workflow attempt cannot enrich the older job's observation. Check its
            // attempt before merging run state, so terminal facts never cross attempt fences.
            if current.run_attempt != verified.run.run_attempt {
                continue;
            }
            if self
                .journal
                .run_is_cancelled(
                    verified.base_repository.id,
                    verified.run.id,
                    current.run_attempt,
                )
                .map_err(|_| IngressError::Journal)?
            {
                continue;
            }
            let mut observation = verified.observation.clone();
            observation.run_status = current.status.clone();
            observation.run_conclusion = current.conclusion.clone();
            if self
                .journal
                .record_job_observation(&JobObservationRecord {
                    repository_id: observation.repository_id,
                    repository_owner: observation.repository_owner.clone(),
                    repository_name: observation.repository_name.clone(),
                    run_id: observation.run_id,
                    run_attempt: observation.run_attempt,
                    job_id: observation.job_id,
                    head_sha: observation.head_sha.clone(),
                    status: observation.status.clone(),
                    conclusion: observation.conclusion.clone(),
                    run_status: observation.run_status,
                    run_conclusion: observation.run_conclusion,
                    runner_name: observation.runner_name,
                    runner_id: observation.runner_id,
                    started_at: observation.started_at,
                    completed_at: observation.completed_at,
                })
                .is_err()
            {
                continue;
            }
            match self.maybe_rerun_interrupted_job(&verified).await {
                Ok(true) => rerun_count += 1,
                Ok(false) => {}
                Err(true) => unresolved = true,
                Err(false) => {}
            }
        }
        if unresolved {
            Err(IngressError::GitHub)
        } else {
            Ok(rerun_count)
        }
    }

    /// Return authenticated runner assignment data observed from the exact GitHub job API.
    ///
    /// # Errors
    /// Returns an error when validation, provenance, persistence, or a dependency request fails.
    pub fn job_observation(
        &self,
        repository_id: u64,
        run_id: u64,
        attempt: u32,
        job_id: u64,
    ) -> Result<Option<JobObservation>, IngressError> {
        self.journal
            .job_observation(repository_id, run_id, attempt, job_id)
            .map(|record| {
                record.map(|value| JobObservation {
                    repository_id: value.repository_id,
                    repository_owner: value.repository_owner,
                    repository_name: value.repository_name,
                    run_id: value.run_id,
                    run_attempt: value.run_attempt,
                    job_id: value.job_id,
                    head_sha: value.head_sha,
                    status: value.status,
                    conclusion: value.conclusion,
                    run_status: value.run_status,
                    run_conclusion: value.run_conclusion,
                    runner_name: value.runner_name,
                    runner_id: value.runner_id,
                    started_at: value.started_at,
                    completed_at: value.completed_at,
                })
            })
            .map_err(|_| IngressError::Journal)
    }
}
