use super::catalog::configured_catalog_fingerprints;
use super::cleanup::valid_bootstrap_labels;
use super::history::candidate_history_key;
use super::native::stored_provider_instance;
use super::selection::purchasing_capability;
use super::selection::selected_image_supports_hook;
use super::selection::selection_matches_binding;
use super::{
    AllocationAdmission, AllocationError, AllocationExecution, AllocationService,
    BootstrapInstance, BootstrapPlatform, BootstrapProvider, BootstrapTarget, CatalogOffer,
    ChildAttemptRecord, ChildCreateResult, ConfigPatch, CreateHistoryRecord, CreateOutcome,
    Duration, File, Instant, JournalError, LocalFailure, NativeIdentityLookup, PoolSnapshotId,
    ProviderBinding, ProviderError, ProviderRequest, ProviderRequirements, ProviderResult,
    RankingResult, SnapshotDocument, SystemTime, UNIX_EPOCH, Value, generate_provider_hook,
    unix_now, unix_now_millis,
};
use crate::providers::provider_pin_json;
use sha2::Digest;
use std::io::Read;

impl AllocationService {
    pub(super) async fn create(
        &self,
        request: ProviderRequest,
    ) -> Result<CreateOutcome, AllocationError> {
        let bootstrap = request
            .bootstrap
            .as_ref()
            .ok_or(AllocationError::Conflict)?;
        let snapshot_id = PoolSnapshotId::resolve(
            request.pool_extra_specs.as_ref(),
            bootstrap.extra_specs.as_ref(),
        )
        .map_err(|_| AllocationError::Snapshot)?
        .ok_or(AllocationError::Snapshot)?;
        let snapshot = self
            .journal
            .pool_snapshot_by_id(snapshot_id.as_str())
            .map_err(|_| AllocationError::Journal)?
            .ok_or(AllocationError::Snapshot)?;
        let document: SnapshotDocument =
            serde_json::from_str(&snapshot.config_json).map_err(|_| AllocationError::Snapshot)?;
        let ingress = document
            .central
            .ingress
            .as_ref()
            .ok_or(AllocationError::Snapshot)?;
        if snapshot.state != "enabled"
            || snapshot.expires_at <= unix_now()
            || self
                .journal
                .run_is_terminal(
                    document.base_repository_id,
                    document.run_id,
                    document.run_attempt,
                )
                .map_err(|_| AllocationError::Journal)?
            || ingress.controller_id != request.controller_id
            || snapshot.garm_pool_id.as_deref() != request.pool_id.as_deref()
            || bootstrap.pool_id != request.pool_id.as_deref().unwrap_or_default()
            || !valid_bootstrap_labels(
                bootstrap,
                &snapshot.request_token,
                &request.controller_id,
                request.pool_id.as_deref().unwrap_or_default(),
            )
        {
            return Err(AllocationError::Snapshot);
        }
        let snapshot_fingerprints = configured_catalog_fingerprints(&document.central)?;
        if snapshot_fingerprints.iter().any(|fingerprint| {
            !self
                .catalogs
                .iter()
                .any(|catalog| &catalog.configuration_fingerprint == fingerprint)
        }) {
            return Err(AllocationError::Catalog);
        }

        if let Some(existing) = self
            .journal
            .allocation(&request.controller_id, &bootstrap.name)
            .map_err(|_| AllocationError::Journal)?
            .filter(|existing| existing.state == "created" && !existing.tombstone)
        {
            if existing.snapshot_id != snapshot.id
                || existing.pool_id != snapshot.garm_pool_id.as_deref().unwrap_or_default()
            {
                return Err(AllocationError::Conflict);
            }

            return stored_provider_instance(&existing).map(CreateOutcome::Created);
        }

        if !crate::admission::allocation_is_open(self.config.admission.as_ref()) {
            return Err(AllocationError::AdmissionClosed);
        }

        let (allocation, is_new) = self
            .journal
            .reserve_allocation(
                &new_allocation_id(&request.controller_id, &bootstrap.name),
                &request.controller_id,
                &bootstrap.name,
                &snapshot.id,
                request
                    .pool_id
                    .as_deref()
                    .ok_or(AllocationError::Snapshot)?,
            )
            .map_err(|error| match error {
                JournalError::AllocationIdentityDeleted => AllocationError::DeletedName,
                _ => AllocationError::Journal,
            })?;
        if !is_new {
            if allocation.snapshot_id != snapshot.id
                || allocation.pool_id != snapshot.garm_pool_id.as_deref().unwrap_or_default()
            {
                return Err(AllocationError::Conflict);
            }
            if allocation.tombstone {
                return Err(AllocationError::Ambiguous);
            }
            if allocation.state == "created" {
                return stored_provider_instance(&allocation).map(CreateOutcome::Created);
            }
            if allocation.state == "dispatched"
                || allocation.state == "ambiguous"
                || allocation.dispatched_at.is_some()
            {
                return Err(AllocationError::Ambiguous);
            }
        }
        if !self
            .journal
            .claim_allocation_selection(&allocation.allocation_id)
            .map_err(|_| AllocationError::Journal)?
        {
            return Err(AllocationError::Ambiguous);
        }

        let result = async {
            self.check_controller_and_pool_caps(&request.controller_id, &snapshot)?;

            let mut profile = document.resolved_profile.clone();
            profile.administrator_limits = self.limits.clone();
            let central = &document.central;
            let allocation_seconds = ingress.allocation_timeout_seconds;
            let bootstrap_seconds = u64::from(ingress.bootstrap_timeout_minutes)
                .checked_mul(60)
                .ok_or(AllocationError::Configuration)?;
            let margin_seconds = ingress.timeout_margin_seconds;
            if allocation_seconds <= margin_seconds || bootstrap_seconds <= margin_seconds {
                return Err(AllocationError::Configuration);
            }
            let deadline = Instant::now()
                + Duration::from_secs(allocation_seconds.min(bootstrap_seconds) - margin_seconds);

            let analysis = self
                .rank_offers(&profile, central, &snapshot_fingerprints, deadline)
                .await?;
            if analysis.quote_missing {
                self.journal
                    .set_allocation_detail(
                        &allocation.allocation_id,
                        Some("price quote unavailable"),
                    )
                    .map_err(|_| AllocationError::Journal)?;
            }
            let ranking = analysis.ranking.ok_or(AllocationError::NoCandidate)?;

            let execution = AllocationExecution {
                request: &request,
                snapshot: &snapshot,
                document: &document,
                bootstrap,
                profile: &profile,
                allocation: &allocation,
                deadline,
            };
            self.dispatch_ranked_candidates(&execution, &analysis.offers, &ranking)
                .await
        }
        .await;
        if result.is_err() {
            let current = self
                .journal
                .allocation(&request.controller_id, &allocation.allocation_id)
                .map_err(|_| AllocationError::Journal)?;
            if current
                .as_ref()
                .is_some_and(|current| current.dispatched_at.is_some())
            {
                self.journal
                    .schedule_allocation_cleanup(
                        &allocation.allocation_id,
                        "dispatched create did not produce a usable provider result",
                    )
                    .map_err(|_| AllocationError::Journal)?;
            }
            self.journal
                .close_unspawned_allocation(&allocation.allocation_id)
                .map_err(|_| AllocationError::Journal)?;
        }

        result
    }

    pub(super) async fn dispatch_ranked_candidates(
        &self,
        execution: &AllocationExecution<'_>,
        offers: &[(CatalogOffer, String)],
        ranking: &RankingResult,
    ) -> Result<CreateOutcome, AllocationError> {
        let AllocationExecution {
            request,
            snapshot,
            document,
            bootstrap,
            profile,
            allocation,
            deadline,
        } = execution;
        let central = &document.central;
        let mut attempt_number = u32::try_from(
            self.journal
                .child_attempts(&allocation.allocation_id)
                .map_err(|_| AllocationError::Journal)?
                .len(),
        )
        .map_err(|_| AllocationError::Journal)?;
        for explanation in ranking.candidates.iter().filter(|entry| entry.eligible) {
            let (offer, configuration_fingerprint) = offers
                .iter()
                .find(|(offer, _fingerprint)| offer.candidate.identity == explanation.identity)
                .ok_or(AllocationError::NoCandidate)?;
            let binding = central
                .provider_bindings
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|binding| binding.id == offer.selection.binding_id)
                .filter(|binding| selection_matches_binding(&offer.selection, binding))
                .ok_or(AllocationError::Configuration)?;
            let account_scope = self.native_account_scope(offer, configuration_fingerprint)?;
            let mapped_bootstrap = match map_bootstrap(bootstrap, offer, central) {
                Ok(mapped) => mapped,
                Err(AllocationError::NoCandidate) => continue,
                Err(error) => return Err(error),
            };
            let mut child_request = (*request).clone();
            child_request.pool_extra_specs = None;
            child_request.instance_id = None;
            child_request.bootstrap = Some(mapped_bootstrap);
            let requirements = ProviderRequirements {
                os: profile.os.clone(),
                architecture: profile.architecture.clone(),
                purchasing: purchasing_capability(&offer.candidate.purchasing)?,
            };
            let offer_json =
                serde_json::to_string(offer).map_err(|_| AllocationError::Configuration)?;
            let binding_json =
                serde_json::to_string(binding).map_err(|_| AllocationError::Configuration)?;
            if !crate::admission::allocation_is_open(self.config.admission.as_ref()) {
                return Err(AllocationError::AdmissionClosed);
            }

            self.journal
                .set_allocation_selection(
                    &allocation.allocation_id,
                    &offer_json,
                    &binding_json,
                    &account_scope,
                    configuration_fingerprint,
                )
                .map_err(|_| AllocationError::Journal)?;
            self.check_account_cap(&request.controller_id, snapshot, &account_scope)?;

            attempt_number = attempt_number.saturating_add(1);
            self.journal
                .insert_child_attempt(&ChildAttemptRecord {
                    allocation_id: allocation.allocation_id.clone(),
                    attempt_number,
                    state: "reserved".into(),
                    offer_json,
                    binding_json,
                    detail: None,
                    created_at: unix_now(),
                    dispatched_at: None,
                    completed_at: None,
                })
                .map_err(|_| AllocationError::Journal)?;
            let dispatch_snapshot = self
                .journal
                .pool_snapshot_by_id(&snapshot.id)
                .map_err(|_| AllocationError::Journal)?
                .ok_or(AllocationError::Snapshot)?;
            let terminal = self
                .journal
                .run_is_terminal(
                    document.base_repository_id,
                    document.run_id,
                    document.run_attempt,
                )
                .map_err(|_| AllocationError::Journal)?;
            if dispatch_snapshot.state != "enabled"
                || dispatch_snapshot.expires_at <= unix_now()
                || terminal
            {
                self.journal
                    .finish_child_attempt(
                        &allocation.allocation_id,
                        attempt_number,
                        "cancelled",
                        "snapshot admission closed before dispatch",
                    )
                    .map_err(|_| AllocationError::Journal)?;
                return Err(AllocationError::Snapshot);
            }
            if !crate::admission::allocation_is_open(self.config.admission.as_ref()) {
                return Err(AllocationError::AdmissionClosed);
            }

            let dispatched = self
                .journal
                .mark_child_dispatched(
                    &allocation.allocation_id,
                    attempt_number,
                    &AllocationAdmission {
                        snapshot_id: snapshot.id.clone(),
                        repository_id: document.base_repository_id,
                        run_id: document.run_id,
                        run_attempt: document.run_attempt,
                        now: unix_now(),
                    },
                )
                .map_err(|_| AllocationError::Journal)?;
            if !dispatched {
                self.journal
                    .finish_child_attempt(
                        &allocation.allocation_id,
                        attempt_number,
                        "cancelled",
                        "allocation tombstoned before dispatch",
                    )
                    .map_err(|_| AllocationError::Journal)?;
                let current = self
                    .journal
                    .allocation(&request.controller_id, &allocation.allocation_id)
                    .map_err(|_| AllocationError::Journal)?
                    .ok_or(AllocationError::Journal)?;
                if current.tombstone {
                    self.journal
                        .set_allocation_state(&allocation.allocation_id, "deleted", None)
                        .map_err(|_| AllocationError::Journal)?;
                    return Err(AllocationError::Conflict);
                }
                return Err(AllocationError::Ambiguous);
            }
            let child_started = Instant::now();
            let child_result = self
                .invoke_pinned(binding, &child_request, &requirements, *deadline)
                .await;
            let child_elapsed_nanos = u64::try_from(child_started.elapsed().as_nanos())
                .ok()
                .filter(|nanos| *nanos > 0);
            if let Some(outcome) = self
                .apply_child_result(
                    execution,
                    offer,
                    configuration_fingerprint,
                    binding,
                    attempt_number,
                    ChildCreateResult {
                        result: child_result,
                        elapsed_nanos: child_elapsed_nanos,
                    },
                )
                .await?
            {
                return Ok(outcome);
            }
        }
        Err(AllocationError::NoCandidate)
    }

    pub(super) async fn apply_child_result(
        &self,
        execution: &AllocationExecution<'_>,
        offer: &CatalogOffer,
        configuration_fingerprint: &str,
        binding: &ProviderBinding,
        attempt_number: u32,
        child_result: ChildCreateResult,
    ) -> Result<Option<CreateOutcome>, AllocationError> {
        let AllocationExecution {
            request,
            bootstrap,
            allocation,
            deadline,
            ..
        } = execution;
        match child_result.result {
            Ok(ProviderResult::Instance(instance)) if !instance.provider_id.is_empty() => {
                let mut instance = instance;
                instance.provider_fault = None;
                let instance_json =
                    serde_json::to_string(&instance).map_err(|_| AllocationError::Configuration)?;
                self.journal
                    .preserve_ambiguous_child_result(
                        &allocation.allocation_id,
                        &instance_json,
                        "positive child return awaiting native ownership",
                    )
                    .map_err(|_| AllocationError::Journal)?;
                let key = candidate_history_key(offer, configuration_fingerprint, binding)?;
                self.journal
                    .finish_child_create(
                        attempt_number,
                        &CreateHistoryRecord {
                            allocation_id: allocation.allocation_id.clone(),
                            candidate_key: key,
                            catalog_fingerprint: configuration_fingerprint.to_owned(),
                            provider_pin_json: provider_pin_json(binding),
                            observed_at_millis: unix_now_millis(),
                            latency_nanos: child_result.elapsed_nanos,
                        },
                    )
                    .map_err(|_| AllocationError::Journal)?;

                let native_identity =
                    Box::pin(self.resolve_native_identity(NativeIdentityLookup {
                        controller_id: &request.controller_id,
                        pool_id: &allocation.pool_id,
                        offer,
                        configuration_fingerprint,
                        runner_name: &bootstrap.name,
                        child_instance_id: Some(&instance.provider_id),
                        deadline: *deadline,
                    }))
                    .await;
                match native_identity {
                    Ok(Some(identity)) => {
                        if !self
                            .journal
                            .claim_allocation_native_identity(&allocation.allocation_id, &identity)
                            .map_err(|_| AllocationError::Journal)?
                        {
                            self.mark_ambiguous(
                                allocation,
                                attempt_number,
                                "native identity is already claimed by another allocation",
                            )?;
                            self.journal
                                .set_allocation_detail(
                                    &allocation.allocation_id,
                                    Some(
                                        "native identity is already claimed by another allocation",
                                    ),
                                )
                                .map_err(|_| AllocationError::Journal)?;
                            return Err(AllocationError::Ambiguous);
                        }

                        self.journal
                            .complete_allocation(
                                &allocation.allocation_id,
                                &instance_json,
                                &serde_json::to_string(&identity)
                                    .map_err(|_| AllocationError::Journal)?,
                            )
                            .map_err(|_| AllocationError::Journal)?;
                    }
                    Ok(None) | Err(_) => {
                        self.journal
                            .set_allocation_detail(
                                &allocation.allocation_id,
                                Some("native identity could not be observed"),
                            )
                            .map_err(|_| AllocationError::Journal)?;
                    }
                }

                let current = self
                    .journal
                    .allocation(&request.controller_id, &allocation.allocation_id)
                    .map_err(|_| AllocationError::Journal)?
                    .ok_or(AllocationError::Journal)?;
                if current.tombstone {
                    return Ok(Some(CreateOutcome::Tombstoned {
                        allocation: Box::new(current),
                        deadline: *deadline,
                    }));
                }

                if current.state != "created" {
                    return Err(AllocationError::Ambiguous);
                }

                instance.provider_id = allocation.allocation_id.clone();
                return serde_json::to_value(instance)
                    .map(CreateOutcome::Created)
                    .map(Some)
                    .map_err(|_| AllocationError::Configuration);
            }
            Ok(_) => {
                self.mark_ambiguous(allocation, attempt_number, "invalid child create result")?;
                return Err(AllocationError::Ambiguous);
            }
            Err(ProviderError::Local(local)) => {
                self.journal
                    .finish_child_attempt(
                        &allocation.allocation_id,
                        attempt_number,
                        "local_failed",
                        local_failure_label(&local),
                    )
                    .map_err(|_| AllocationError::Journal)?;
                self.journal
                    .reset_local_allocation(&allocation.allocation_id)
                    .map_err(|_| AllocationError::Journal)?;
                if local == LocalFailure::AdmissionClosed {
                    return Err(AllocationError::AdmissionClosed);
                }
            }
            Err(_) => {
                self.mark_ambiguous(
                    allocation,
                    attempt_number,
                    "provider create outcome is uncertain",
                )?;
                return Err(AllocationError::Ambiguous);
            }
        }
        Ok(None)
    }
}

pub(super) fn new_allocation_id(controller: &str, runner: &str) -> String {
    let mut random = [0_u8; 16];
    if File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random))
        .is_err()
    {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let hash = sha2::Sha256::digest(format!("{controller}:{runner}:{now}").as_bytes());
        random.copy_from_slice(&hash[..16]);
    }
    format!(
        "outrider-{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

pub(super) fn map_bootstrap(
    original: &BootstrapInstance,
    offer: &CatalogOffer,
    config: &ConfigPatch,
) -> Result<BootstrapInstance, AllocationError> {
    let (provider, platform) = match (
        offer.candidate.cloud.as_str(),
        offer.candidate.os.as_str(),
        offer.candidate.architecture.as_str(),
    ) {
        ("aws", "linux", "amd64") => (BootstrapProvider::Aws, BootstrapPlatform::LinuxAmd64),
        ("aws", "linux", "arm64") => (BootstrapProvider::Aws, BootstrapPlatform::LinuxArm64),
        ("aws", "windows", "amd64") => (BootstrapProvider::Aws, BootstrapPlatform::WindowsAmd64),
        ("azure", "linux", "amd64") => (BootstrapProvider::Azure, BootstrapPlatform::LinuxAmd64),
        ("azure", "linux", "arm64") => (BootstrapProvider::Azure, BootstrapPlatform::LinuxArm64),
        ("azure", "windows", "amd64") => {
            (BootstrapProvider::Azure, BootstrapPlatform::WindowsAmd64)
        }
        ("gcp", "linux", "amd64") => (BootstrapProvider::Gcp, BootstrapPlatform::LinuxAmd64),
        ("gcp", "linux", "arm64") => (BootstrapProvider::Gcp, BootstrapPlatform::LinuxArm64),
        ("gcp", "windows", "amd64") => (BootstrapProvider::Gcp, BootstrapPlatform::WindowsAmd64),
        (_, "macos" | "osx" | "darwin", _) => {
            return Err(AllocationError::NoCandidate);
        }
        _ => return Err(AllocationError::NoCandidate),
    };
    let mut child = original.clone();
    child.flavor = offer.selection.flavor.clone();
    child.image = offer.selection.image.clone();
    child.os_type = offer.candidate.os.clone();
    child.os_arch = offer.candidate.architecture.clone();
    let hook = generate_provider_hook(
        &child,
        BootstrapTarget {
            platform,
            macos_user_data_enabled: false,
        },
        provider,
    )
    .map_err(|_| AllocationError::NoCandidate)?;
    if hook.requires_cloud_init && !selected_image_supports_hook(config, offer) {
        return Err(AllocationError::NoCandidate);
    }
    child.user_data_options = hook.user_data_options.clone();
    let mut extras = hook
        .extra_specs_json()
        .as_object()
        .cloned()
        .ok_or(AllocationError::Configuration)?;
    for (name, value) in &offer.selection.approved_extras {
        if name == "snapshot_id" {
            continue;
        }
        if extras.contains_key(name) {
            return Err(AllocationError::Configuration);
        }
        extras.insert(name.clone(), value.clone());
    }
    child.extra_specs = Some(Value::Object(extras));
    Ok(child)
}

pub(super) fn local_failure_label(error: &LocalFailure) -> &'static str {
    match error {
        LocalFailure::InvalidRegistry => "invalid_registry",
        LocalFailure::UnknownBinding => "unknown_binding",
        LocalFailure::UnsupportedRequest => "unsupported_request",
        LocalFailure::CapabilityUnavailable => "capability_unavailable",
        LocalFailure::PinValidation => "pin_validation",
        LocalFailure::VersionMismatch => "version_mismatch",
        LocalFailure::Spawn => "spawn_failed_before_child_start",
        LocalFailure::InputLimit => "input_limit",
        LocalFailure::InvalidResult => "invalid_result",
        LocalFailure::OutputLimit => "output_limit",
        LocalFailure::Supervision => "supervision_failed",
        LocalFailure::AdmissionClosed => "administrator allocation admission is closed",
        LocalFailure::Deadline => "deadline_before_dispatch",
        LocalFailure::UnsupportedHost => "unsupported_host",
    }
}
