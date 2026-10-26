use super::*;
use reqwest::{StatusCode, Url};

impl GitHubClient {
    /// Create an authenticated client pinned to the administrator's API origin.
    ///
    /// # Errors
    /// Returns an error when validation, provenance, persistence, or a dependency request fails.
    pub fn from_token_file(
        origin: &str,
        token_file: &str,
        timeout: Duration,
    ) -> Result<Self, GitHubError> {
        let origin = Url::parse(origin).map_err(|_| GitHubError::Configuration)?;
        if !matches!(origin.scheme(), "http" | "https")
            || origin.host_str().is_none()
            || origin.username() != ""
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || !origin.path().ends_with('/')
            || timeout.is_zero()
        {
            return Err(GitHubError::Configuration);
        }
        let token = fs::read_to_string(token_file).map_err(|_| GitHubError::Configuration)?;
        if token.trim().is_empty() {
            return Err(GitHubError::Configuration);
        }
        let client = Client::builder()
            .user_agent(concat!(
                "garm-provider-outrider/",
                env!("CARGO_PKG_VERSION")
            ))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| GitHubError::Configuration)?;
        Ok(Self {
            client,
            origin,
            token_file: token_file.to_owned(),
            timeout,
        })
    }

    /// Verify one raw body with the registered repository secret.
    ///
    /// # Errors
    /// Returns an error when validation, provenance, persistence, or a dependency request fails.
    pub fn verify_signature(
        secret: &[u8],
        signature: &str,
        body: &[u8],
    ) -> Result<(), GitHubError> {
        let digest = signature
            .strip_prefix("sha256=")
            .ok_or(GitHubError::Signature)?;
        let bytes = hex::decode(digest).map_err(|_| GitHubError::Signature)?;
        let mut mac = HookMac::new_from_slice(secret).map_err(|_| GitHubError::Signature)?;
        mac.update(body);
        mac.verify_slice(&bytes).map_err(|_| GitHubError::Signature)
    }

    /// Fetch exact run, attempt jobs, and the identified job; reject any disagreement.
    ///
    /// # Errors
    /// Returns an error when validation, provenance, persistence, or a dependency request fails.
    pub async fn verify_job(
        &self,
        event: &WorkflowJobEvent,
        allow_forks: bool,
    ) -> Result<VerifiedJob, GitHubError> {
        let attempt = event
            .workflow_job
            .run_attempt
            .filter(|attempt| *attempt > 0)
            .ok_or(GitHubError::Provenance)?;
        if event.workflow_job.id == 0
            || event.workflow_job.run_id == 0
            || event.repository.id == 0
            || !matches!(
                event.workflow_job.status.as_str(),
                "queued" | "waiting" | "in_progress" | "completed"
            )
            || event.action.as_deref() != Some(event.workflow_job.status.as_str())
            || !valid_sha(&event.workflow_job.head_sha)
        {
            return Err(GitHubError::Provenance);
        }
        let base = format!(
            "repos/{}/{}",
            path_component(&event.repository.owner.login),
            path_component(&event.repository.name)
        );
        let run: WorkflowRun = self
            .get_json(&format!(
                "{base}/actions/runs/{}/attempts/{attempt}",
                event.workflow_job.run_id
            ))
            .await?;
        if run.id != event.workflow_job.run_id
            || run.run_attempt != attempt
            || run.repository.id != event.repository.id
            || run.repository.full_name != event.repository.full_name
            || run.head_sha != event.workflow_job.head_sha
            || !valid_sha(&run.head_sha)
        {
            return Err(GitHubError::Provenance);
        }
        if !valid_run_state(&run) {
            return Err(GitHubError::Request);
        }

        let config_repository = match run.event.as_str() {
            "pull_request" => {
                let head = run.head_repository.clone().ok_or(GitHubError::Provenance)?;
                if head.id != run.repository.id && !allow_forks {
                    return Err(GitHubError::Provenance);
                }
                head
            }
            "pull_request_target" if !allow_forks => return Err(GitHubError::Provenance),
            "push" | "merge_group" | "pull_request_target" => run.repository.clone(),
            "workflow_dispatch" | "schedule" | "release" | "repository_dispatch" => {
                if run
                    .head_repository
                    .as_ref()
                    .is_some_and(|head| head != &run.repository)
                {
                    return Err(GitHubError::Provenance);
                }
                run.repository.clone()
            }
            _ => return Err(GitHubError::Provenance),
        };

        let job_path = format!(
            "{base}/actions/runs/{}/attempts/{attempt}/jobs?per_page=100&page=1",
            event.workflow_job.run_id
        );
        let jobs = self.get_pages::<WorkflowJob>(&job_path, "jobs").await?;
        let matching: Vec<WorkflowJob> = jobs
            .into_iter()
            .flatten()
            .filter(|job| job.id == event.workflow_job.id)
            .collect();
        if matching.len() != 1
            || !same_job_identity(&matching[0], event, attempt, &run)
            || !valid_job_state(&matching[0].status, matching[0].conclusion.as_deref())
        {
            return Err(GitHubError::Provenance);
        }
        let job: WorkflowJob = self
            .get_json(&format!(
                "repos/{}/actions/jobs/{}",
                repository_path(&event.repository),
                event.workflow_job.id
            ))
            .await?;
        if !same_job_identity(&job, event, attempt, &run) {
            return Err(GitHubError::Provenance);
        }
        if !valid_job_state(&job.status, job.conclusion.as_deref()) {
            return Err(GitHubError::Provenance);
        }
        if matching[0].status == "completed" {
            if job.status != "completed" {
                return Err(GitHubError::Request);
            }
            if matching[0].conclusion != job.conclusion {
                return Err(GitHubError::Provenance);
            }
        }
        if event.workflow_job.status == "completed" && job.status != "completed" {
            return Err(GitHubError::Request);
        }
        if event.workflow_job.status == "completed"
            && job.conclusion != event.workflow_job.conclusion
        {
            return Err(GitHubError::Provenance);
        }

        let observation = JobObservation {
            repository_id: run.repository.id,
            repository_owner: run.repository.owner.login.clone(),
            repository_name: run.repository.name.clone(),
            run_id: run.id,
            run_attempt: attempt,
            job_id: job.id,
            head_sha: job.head_sha.clone(),
            status: job.status.clone(),
            conclusion: job.conclusion.clone(),
            run_status: run.status.clone(),
            run_conclusion: run.conclusion.clone(),
            runner_name: job.runner_name.clone(),
            runner_id: job.runner_id,
            started_at: job.started_at.clone(),
            completed_at: job.completed_at.clone(),
        };
        Ok(VerifiedJob {
            base_repository: run.repository.clone(),
            config_repository,
            run,
            job,
            observation,
        })
    }

    /// Submit the documented exact-job rerun request and accept only its empty 201 response.
    ///
    /// # Errors
    /// Returns an error when authentication, transport, or the GitHub response fails.
    async fn rerun_job(
        &self,
        repository: &RepositoryRef,
        run_id: u64,
        run_attempt: u32,
        job_id: u64,
    ) -> Result<RerunSubmission, GitHubError> {
        if job_id == 0 {
            return Err(GitHubError::Provenance);
        }
        let path = format!(
            "repos/{}/actions/jobs/{job_id}/rerun",
            repository_path(repository)
        );
        let token = self.token().await?;
        let response = timeout(
            self.timeout,
            self.client
                .post(self.resource(&path)?)
                .bearer_auth(token)
                .header("Accept", "application/vnd.github+json")
                .send(),
        )
        .await
        .map_err(|_| GitHubError::Request)?
        .map_err(|_| GitHubError::Request)?;
        match response.status() {
            StatusCode::CREATED => Ok(RerunSubmission::Acknowledged),
            StatusCode::FORBIDDEN => {
                eprintln!(
                    "outrider rerun denied by GitHub (403) for repository {} run {} attempt {} job {}; check Actions write permission and repository policy, then rerun manually in GitHub",
                    repository.id, run_id, run_attempt, job_id
                );
                Ok(RerunSubmission::Denied)
            }
            _ => Err(GitHubError::Request),
        }
    }

    /// Read repository configuration only at the authenticated immutable commit.
    ///
    /// # Errors
    /// Returns an error when validation, provenance, persistence, or a dependency request fails.
    pub async fn repository_config(
        &self,
        repository: &RepositoryRef,
        sha: &str,
    ) -> Result<String, GitHubError> {
        if !valid_sha(sha) || repository.owner.login.is_empty() || repository.name.is_empty() {
            return Err(GitHubError::Contents);
        }
        let commit_path = format!(
            "repos/{}/{}/commits/{sha}",
            path_component(&repository.owner.login),
            path_component(&repository.name)
        );
        let commit_response = self.request(&commit_path).await?;
        if commit_response.status() == StatusCode::NOT_FOUND {
            return Err(GitHubError::Contents);
        }
        let commit: CommitResponse = commit_response
            .error_for_status()
            .map_err(|_| GitHubError::Request)?
            .json()
            .await
            .map_err(|_| GitHubError::Request)?;
        if commit.sha != sha {
            return Err(GitHubError::Contents);
        }
        let path = format!(
            "repos/{}/{}/contents/.outrider.toml?ref={sha}",
            path_component(&repository.owner.login),
            path_component(&repository.name)
        );
        let response = self.request(&path).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok("version = 1\n".to_owned());
        }
        let content: ContentsResponse = response
            .error_for_status()
            .map_err(|_| GitHubError::Contents)?
            .json()
            .await
            .map_err(|_| GitHubError::Contents)?;
        if content.encoding != "base64" || content.size > 1024 * 1024 {
            return Err(GitHubError::Contents);
        }
        String::from_utf8(
            STANDARD
                .decode(content.content.replace('\n', ""))
                .map_err(|_| GitHubError::Contents)?,
        )
        .map_err(|_| GitHubError::Contents)
    }

    /// Fetch an API resource at the configured origin; redirects are never followed.
    ///
    /// # Errors
    /// Returns an error when validation, provenance, persistence, or a dependency request fails.
    pub async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
    ) -> Result<T, GitHubError> {
        let response = self
            .request(path)
            .await?
            .error_for_status()
            .map_err(|_| GitHubError::Request)?;
        response.json().await.map_err(|_| GitHubError::Request)
    }

    async fn get_pages<T: for<'de> Deserialize<'de>>(
        &self,
        first_path: &str,
        field: &str,
    ) -> Result<Vec<Vec<T>>, GitHubError> {
        let mut url = self.resource(first_path)?;
        let mut visited = HashSet::new();
        let mut pages = Vec::new();
        for _ in 0..10 {
            if !visited.insert(url.as_str().to_owned()) {
                return Err(GitHubError::Request);
            }
            let response = self
                .request_url(url)
                .await?
                .error_for_status()
                .map_err(|_| GitHubError::Request)?;
            let next = next_link(response.headers())?
                .map(|link| self.checked_link(link))
                .transpose()?;
            let value: Value = response.json().await.map_err(|_| GitHubError::Request)?;
            let items = value.get(field).cloned().ok_or(GitHubError::Request)?;
            pages.push(serde_json::from_value(items).map_err(|_| GitHubError::Request)?);
            match next {
                Some(next) => url = next,
                None => return Ok(pages),
            }
        }
        Err(GitHubError::Request)
    }

    /// Create an idempotent check-run request through the configured API origin.
    ///
    /// # Errors
    /// Returns an error when validation, provenance, persistence, or a dependency request fails.
    pub async fn create_check(
        &self,
        repository: &RepositoryRef,
        body: Value,
    ) -> Result<(), GitHubError> {
        let sha = body
            .get("head_sha")
            .and_then(Value::as_str)
            .ok_or(GitHubError::Provenance)?;
        let external_id = body
            .get("external_id")
            .and_then(Value::as_str)
            .ok_or(GitHubError::Provenance)?;
        let list_path = format!(
            "repos/{}/commits/{sha}/check-runs?check_name=Outrider%20admission&per_page=100&page=1",
            repository_path(repository)
        );
        let existing = self
            .get_pages::<CheckRunRef>(&list_path, "check_runs")
            .await?;
        let matches = existing
            .into_iter()
            .flatten()
            .filter(|check| check.external_id.as_deref() == Some(external_id))
            .count();
        if matches > 1 {
            return Err(GitHubError::Provenance);
        }
        if matches == 1 {
            return Ok(());
        }
        let path = format!("repos/{}/check-runs", repository_path(repository));
        let token = self.token().await?;
        let response = timeout(
            self.timeout,
            self.client
                .post(self.resource(&path)?)
                .bearer_auth(token)
                .header("Accept", "application/vnd.github+json")
                .json(&body)
                .send(),
        )
        .await
        .map_err(|_| GitHubError::Request)?
        .map_err(|_| GitHubError::Request)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(GitHubError::Request)
        }
    }

    pub(super) async fn rerun_verified_job(
        &self,
        job: &VerifiedJob,
    ) -> Result<RerunSubmission, GitHubError> {
        if job.job.status != "completed"
            || job.job.conclusion.as_deref() != Some("failure")
            || job.job.runner_name.as_deref().is_none_or(str::is_empty)
            || job.job.runner_id.is_none()
            || job.job.started_at.is_none()
            || job.job.completed_at.is_none()
        {
            return Err(GitHubError::Provenance);
        }
        self.rerun_job(
            &job.base_repository,
            job.run.id,
            job.run.run_attempt,
            job.job.id,
        )
        .await
    }

    pub(super) async fn current_run(
        &self,
        repository: &RepositoryRef,
        run_id: u64,
    ) -> Result<WorkflowRun, GitHubError> {
        let run: WorkflowRun = self
            .get_json(&format!(
                "repos/{}/actions/runs/{run_id}",
                repository_path(repository)
            ))
            .await?;
        if run.id != run_id
            || run.repository.id != repository.id
            || run.repository.full_name != repository.full_name
            || run.run_attempt == 0
        {
            return Err(GitHubError::Provenance);
        }
        if !valid_run_state(&run) {
            return Err(GitHubError::Provenance);
        }
        Ok(run)
    }

    async fn request(&self, path: &str) -> Result<reqwest::Response, GitHubError> {
        self.request_url(self.resource(path)?).await
    }

    async fn request_url(&self, url: Url) -> Result<reqwest::Response, GitHubError> {
        if url.origin() != self.origin.origin() {
            return Err(GitHubError::Configuration);
        }
        let token = self.token().await?;
        let response = timeout(
            self.timeout,
            self.client
                .get(url)
                .bearer_auth(token)
                .header("Accept", "application/vnd.github+json")
                .send(),
        )
        .await
        .map_err(|_| GitHubError::Request)?
        .map_err(|_| GitHubError::Request)?;
        if response.status().is_redirection() {
            return Err(GitHubError::Request);
        }
        Ok(response)
    }

    fn resource(&self, path: &str) -> Result<Url, GitHubError> {
        if path.starts_with('/') || path.contains("..") || path.contains("\\") {
            return Err(GitHubError::Configuration);
        }
        let url = self
            .origin
            .join(path)
            .map_err(|_| GitHubError::Configuration)?;
        if url.origin() != self.origin.origin() || !url.path().starts_with(self.origin.path()) {
            return Err(GitHubError::Configuration);
        }
        Ok(url)
    }

    fn checked_link(&self, link: Url) -> Result<Url, GitHubError> {
        if link.origin() != self.origin.origin() || !link.path().starts_with(self.origin.path()) {
            return Err(GitHubError::Configuration);
        }
        Ok(link)
    }

    async fn token(&self) -> Result<String, GitHubError> {
        let token = timeout(self.timeout, tokio::fs::read_to_string(&self.token_file))
            .await
            .map_err(|_| GitHubError::Request)?
            .map_err(|_| GitHubError::Configuration)?;
        if token.trim().is_empty() {
            return Err(GitHubError::Configuration);
        }
        Ok(token.trim().to_owned())
    }
}
