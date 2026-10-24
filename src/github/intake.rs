use super::{
    CheckOutbox, ConfigPatch, ForwardHeaders, GarmClient, GitHubClient, IngressError,
    IngressService, Journal, OwnerRef, RepositoryRef, WebhookDelivery, valid_uuid,
};
use serde_json::Value;
use std::{collections::BTreeMap, fs, sync::Arc, time::Duration};

impl IngressService {
    /// Build the production webhook processor from administrator config and the durable journal.
    ///
    /// # Errors
    /// Returns an error when validation, provenance, persistence, or a dependency request fails.
    pub fn new(config: ConfigPatch, journal: Arc<Journal>) -> Result<Self, IngressError> {
        let settings = config.ingress.clone().ok_or(IngressError::Configuration)?;
        if config.validate_rerun_settings().is_err()
            || config.validate_admission_settings().is_err()
        {
            return Err(IngressError::Configuration);
        }
        if settings.max_dynamic_pools == 0
            || settings.max_runners_per_pool == 0
            || settings.max_runners_per_pool > 1000
            || settings.allocation_timeout_seconds == 0
            || settings.bootstrap_timeout_minutes == 0
            || settings.timeout_margin_seconds == 0
            || settings.pool_ttl_seconds == 0
            || settings.webhook_max_bytes == 0
            || settings.webhook_max_bytes > 10 * 1024 * 1024
            || !valid_uuid(&settings.controller_id)
            || settings.repositories.is_empty()
        {
            return Err(IngressError::Configuration);
        }
        for repository in settings.repositories.values() {
            if !valid_uuid(&repository.garm_repository_id)
                || repository.webhook_secret_file.is_empty()
                || repository.template_id == Some(0)
            {
                return Err(IngressError::Configuration);
            }
        }
        let request_timeout = Duration::from_secs(10);
        let github = GitHubClient::from_token_file(
            &settings.github_api_origin,
            &settings.github_token_file,
            request_timeout,
        )
        .map_err(|_| IngressError::Configuration)?;
        let garm = GarmClient::from_settings(&settings, request_timeout)
            .map_err(|_| IngressError::Configuration)?;
        Ok(Self {
            config,
            settings,
            journal,
            github,
            garm,
            processor_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Authenticate and durably save an original signed delivery before acknowledging it.
    ///
    /// # Errors
    /// Returns an error when validation, provenance, persistence, or a dependency request fails.
    pub fn intake(
        &self,
        event_name: &str,
        delivery_id: &str,
        signature: &str,
        headers: BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<bool, IngressError> {
        if event_name.is_empty()
            || delivery_id.is_empty()
            || body.is_empty()
            || body.len() > self.settings.webhook_max_bytes
        {
            return Err(IngressError::Webhook);
        }
        let payload: Value = serde_json::from_slice(body).map_err(|_| IngressError::Webhook)?;
        let repository_id = payload
            .get("repository")
            .and_then(|repo| repo.get("id"))
            .and_then(Value::as_u64)
            .filter(|id| *id > 0)
            .ok_or(IngressError::Webhook)?;
        let repository = self
            .settings
            .repositories
            .get(&repository_id.to_string())
            .ok_or(IngressError::Webhook)?;
        let secret =
            fs::read(&repository.webhook_secret_file).map_err(|_| IngressError::Configuration)?;
        let secret = secret.trim_ascii();
        if secret.is_empty() {
            return Err(IngressError::Configuration);
        }
        GitHubClient::verify_signature(secret, signature, body)
            .map_err(|_| IngressError::Webhook)?;

        let mut original_headers = headers
            .into_iter()
            .filter_map(|(name, value)| {
                let name = name.to_ascii_lowercase();
                matches!(
                    name.as_str(),
                    "x-github-event"
                        | "x-github-delivery"
                        | "x-github-hook-installation-target-type"
                        | "x-hub-signature-256"
                )
                .then_some((name, value))
            })
            .collect::<BTreeMap<_, _>>();
        if original_headers
            .get("x-github-event")
            .is_some_and(|value| value != event_name)
            || original_headers
                .get("x-github-delivery")
                .is_some_and(|value| value != delivery_id)
            || original_headers
                .get("x-hub-signature-256")
                .is_some_and(|value| value != signature)
        {
            return Err(IngressError::Webhook);
        }
        original_headers.insert("x-github-event".into(), event_name.to_owned());
        original_headers.insert("x-github-delivery".into(), delivery_id.to_owned());
        original_headers.insert("x-hub-signature-256".into(), signature.to_owned());
        let headers_json = serde_json::to_string(&ForwardHeaders(original_headers))
            .map_err(|_| IngressError::Webhook)?;
        let delivery = WebhookDelivery {
            delivery_id: delivery_id.to_owned(),
            repository_id,
            event: event_name.to_owned(),
            headers_json,
            body: body.to_vec(),
            state: "received".into(),
        };
        let terminal = if event_name == "workflow_run"
            && payload.get("action").and_then(Value::as_str) == Some("completed")
        {
            let run = payload.get("workflow_run").ok_or(IngressError::Webhook)?;
            let run_id = run
                .get("id")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0)
                .ok_or(IngressError::Webhook)?;
            let attempt = run
                .get("run_attempt")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or(IngressError::Webhook)?;
            Some((
                run_id,
                attempt,
                run.get("conclusion").and_then(Value::as_str) == Some("cancelled"),
            ))
        } else {
            None
        };
        let inserted = self
            .journal
            .persist_delivery_with_terminal(&delivery, terminal)
            .map_err(|_| IngressError::Journal)?;
        Ok(inserted)
    }

    /// Process bounded durable inbox/outbox work; failures remain retryable and poison rows do not block later rows.
    ///
    /// # Errors
    /// Returns an error when validation, provenance, persistence, or a dependency request fails.
    pub async fn process_pending(&self, limit: usize) -> Result<usize, IngressError> {
        let _guard = self.processor_lock.lock().await;
        let mut processed = 0;
        let deliveries = self
            .journal
            .pending_deliveries(limit)
            .map_err(|_| IngressError::Journal)?;
        for delivery in deliveries {
            let deadline = tokio::time::Instant::now() + self.operation_budget();
            match tokio::time::timeout_at(deadline, self.process_delivery(&delivery)).await {
                Ok(Ok(())) => {
                    self.journal
                        .set_delivery_state(&delivery.delivery_id, "queued", None)
                        .map_err(|_| IngressError::Journal)?;
                    processed += 1;
                }
                Ok(Err(true)) | Err(_) => self
                    .journal
                    .set_delivery_state(
                        &delivery.delivery_id,
                        "retry",
                        Some("dependency unavailable"),
                    )
                    .map_err(|_| IngressError::Journal)?,
                Ok(Err(false)) => {
                    self.journal
                        .set_delivery_state(
                            &delivery.delivery_id,
                            "rejected",
                            Some("delivery rejected"),
                        )
                        .map_err(|_| IngressError::Journal)?;
                    processed += 1;
                }
            }
        }

        self.observe_pending_job_reruns(limit).await?;

        for message in self
            .journal
            .pending_outbox(limit)
            .map_err(|_| IngressError::Journal)?
        {
            let deadline = tokio::time::Instant::now() + self.operation_budget();
            if message.kind == "forward" {
                let current =
                    tokio::time::timeout_at(deadline, self.forward_job_is_current(&message)).await;
                match current {
                    Ok(Ok(true)) => {}
                    Ok(Ok(false)) => {
                        self.journal
                            .set_outbox_state(
                                &message.id,
                                "rejected",
                                Some("admission is terminal"),
                            )
                            .map_err(|_| IngressError::Journal)?;
                        processed += 1;
                        continue;
                    }
                    Ok(Err(false)) => {
                        self.journal
                            .set_outbox_state(
                                &message.id,
                                "rejected",
                                Some("forward does not identify a valid Outrider job"),
                            )
                            .map_err(|_| IngressError::Journal)?;
                        processed += 1;
                        continue;
                    }
                    Ok(Err(true)) | Err(_) => {
                        self.journal
                            .set_outbox_state(&message.id, "retry", Some("dependency unavailable"))
                            .map_err(|_| IngressError::Journal)?;
                        continue;
                    }
                }

                if !self.forward_allocation_is_open(&message) {
                    self.journal
                        .set_outbox_state(
                            &message.id,
                            "retry",
                            Some("administrator allocation admission is closed"),
                        )
                        .map_err(|_| IngressError::Journal)?;
                    continue;
                }
            }
            let dispatch = tokio::time::timeout_at(deadline, async {
                if message.kind == "forward" && !self.forward_allocation_is_open(&message) {
                    return Ok(false);
                }

                match message.kind.as_str() {
                    "forward" => {
                        let headers: ForwardHeaders = serde_json::from_str(&message.headers_json)
                            .map_err(|_| IngressError::Journal)?;
                        Ok::<bool, IngressError>(
                            self.garm
                                .forward_webhook(
                                    &message.destination,
                                    &headers.0.into_iter().collect::<Vec<_>>(),
                                    &message.body,
                                )
                                .await
                                .is_ok(),
                        )
                    }
                    "check" => {
                        let outbox: CheckOutbox = serde_json::from_slice(&message.body)
                            .map_err(|_| IngressError::Journal)?;
                        Ok(self
                            .github
                            .create_check(&outbox.repository, outbox.check)
                            .await
                            .is_ok())
                    }
                    _ => Err(IngressError::Webhook),
                }
            })
            .await;
            let (state, detail, sent) = match dispatch {
                Ok(Ok(true)) => ("delivered", None, true),
                Ok(Ok(false)) | Err(_) => ("retry", Some("dependency unavailable"), false),
                Ok(Err(_)) => ("rejected", Some("outbox record is malformed"), false),
            };
            self.journal
                .set_outbox_state(&message.id, state, detail)
                .map_err(|_| IngressError::Journal)?;
            if sent {
                processed += 1;
            }
        }
        processed += self.drain_expired_pools(limit).await?;
        Ok(processed)
    }

    pub(super) async fn observe_pending_job_reruns(
        &self,
        limit: usize,
    ) -> Result<(), IngressError> {
        for intent in self
            .journal
            .pending_job_reruns(limit)
            .map_err(|_| IngressError::Journal)?
        {
            if intent.github_api_origin != self.settings.github_api_origin {
                eprintln!(
                    "outrider rerun intent fenced for repository {} run {}: GitHub API origin changed",
                    intent.repository_id, intent.run_id
                );
                self.journal
                    .set_job_rerun_state(
                        intent.repository_id,
                        intent.run_id,
                        intent.run_attempt,
                        intent.job_id,
                        "transport_changed_fenced",
                    )
                    .map_err(|_| IngressError::Journal)?;
                continue;
            }
            if self
                .journal
                .run_is_cancelled(intent.repository_id, intent.run_id, intent.run_attempt)
                .map_err(|_| IngressError::Journal)?
            {
                self.journal
                    .set_job_rerun_state(
                        intent.repository_id,
                        intent.run_id,
                        intent.run_attempt,
                        intent.job_id,
                        "cancellation_veto",
                    )
                    .map_err(|_| IngressError::Journal)?;
                continue;
            }
            let repository = RepositoryRef {
                id: intent.repository_id,
                name: intent.repository.clone(),
                full_name: format!("{}/{}", intent.owner, intent.repository),
                owner: OwnerRef {
                    login: intent.owner,
                },
            };
            if let Ok(run) = self.github.current_run(&repository, intent.run_id).await {
                let cancelled = run.conclusion.as_deref() == Some("cancelled");
                if cancelled {
                    self.journal
                        .mark_run_cancelled(intent.repository_id, intent.run_id, run.run_attempt)
                        .map_err(|_| IngressError::Journal)?;
                }
                let intent_state = if run.run_attempt > intent.run_attempt {
                    Some("later_attempt_observed")
                } else if cancelled {
                    Some("cancellation_veto")
                } else {
                    None
                };
                if let Some(intent_state) = intent_state {
                    self.journal
                        .set_job_rerun_state(
                            intent.repository_id,
                            intent.run_id,
                            intent.run_attempt,
                            intent.job_id,
                            intent_state,
                        )
                        .map_err(|_| IngressError::Journal)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn operation_budget(&self) -> Duration {
        Duration::from_secs(
            self.settings
                .allocation_timeout_seconds
                .saturating_add(self.settings.timeout_margin_seconds),
        )
    }
}
