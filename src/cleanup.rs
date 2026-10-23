use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    config::ConfigPatch,
    github::SnapshotDocument,
    journal::{CleanupResolutionAudit, CleanupResolutionScope, Journal},
    policy::{CatalogOffer, ProviderLaunchFacts},
    providers::ProviderBinding,
};

const ATTESTATION_MAX_AGE_SECONDS: u64 = 60 * 60;
const EVIDENCE_REFERENCE_MAX_BYTES: usize = 512;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CleanupAttestation {
    pub schema_version: u32,
    pub allocation_id: String,
    pub controller_id: String,
    pub pool_id: String,
    pub runner_name: String,
    pub scope_digest: String,
    pub evidence_reference: String,
    pub verified_at_unix_seconds: u64,
    pub acknowledgements: CleanupAcknowledgements,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CleanupAcknowledgements {
    pub all_cloud_operations_terminal: bool,
    pub no_local_provider_process_can_submit_work: bool,
    pub all_resources_and_dependencies_proved_absent: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CleanupInspection {
    pub schema_version: u32,
    pub allocation_id: String,
    pub controller_id: String,
    pub pool_id: String,
    pub runner_name: String,
    pub snapshot_id: String,
    pub catalog_id: String,
    pub catalog_fingerprint: String,
    pub cloud: String,
    pub logical_account_id: String,
    pub logical_location: String,
    pub native_account_id: String,
    pub native_location: String,
    pub launch_scope: CleanupLaunchScope,
    pub provider_binding_id: String,
    pub provider_source_revision: String,
    pub provider_binary_sha256: String,
    pub provider_configuration_sha256: String,
    pub native_resource_id: Option<String>,
    pub native_host_id: Option<String>,
    pub native_dependencies: std::collections::BTreeMap<String, String>,
    pub native_identity_known: bool,
    pub allocation_state: String,
    pub tombstone: bool,
    pub created_at: u64,
    pub dispatched_at: u64,
    pub returned_at: Option<u64>,
    pub cleanup_action_id: i64,
    pub cleanup_action_state: String,
    pub cleanup_attempt_count: u32,
    pub cleanup_last_attempt_at: Option<u64>,
    pub cleanup_next_attempt_at: Option<u64>,
    pub unresolved_actions: Vec<CleanupActionSummary>,
    pub scope_digest: String,
    #[serde(default)]
    pub historical_audit: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CleanupLaunchScope {
    pub image_alias: String,
    pub native_image_id: String,
    pub availability_zone: Option<String>,
    pub subnet_id: Option<String>,
    pub vnet_subnet_id: Option<String>,
    pub network_id: Option<String>,
    pub subnetwork_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CleanupActionSummary {
    pub action_id: i64,
    pub command: String,
    pub state: String,
    pub started_at: u64,
    pub unresolved_create: bool,
    pub reusable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RetainedNativeScope {
    catalog_id: String,
    cloud: String,
    account_id: String,
    location: String,
    launch_scope: CleanupLaunchScope,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CleanupResolutionSummary {
    attestation: CleanupAttestation,
    inspection: CleanupInspection,
}

#[derive(Debug, thiserror::Error)]
pub enum CleanupError {
    #[error("cleanup scope is missing or inconsistent")]
    Scope,
    #[error("cleanup attestation is invalid or stale")]
    Attestation,
    #[error("cleanup resolution conflicts with prior audit")]
    Conflict,
    #[error("cleanup journal operation failed: {0}")]
    Journal(#[from] crate::journal::JournalError),
    #[error("cleanup serialization failed")]
    Serialization,
    #[error("cleanup commands require a Unix host with provider process-group support")]
    UnsupportedHost,
}

/// Inspect the retained offline cleanup scope without contacting a provider.
///
/// # Errors
///
/// Returns an error when the host is unsupported or retained scope is missing,
/// inconsistent, or unreadable.
pub fn inspect(
    journal: &Journal,
    controller_id: &str,
    allocation_id: &str,
) -> Result<CleanupInspection, CleanupError> {
    validate_host()?;
    if let Some(summary_json) = journal.cleanup_resolution_summary_json(allocation_id)? {
        let mut summary: CleanupResolutionSummary =
            serde_json::from_str(&summary_json).map_err(|_| CleanupError::Serialization)?;
        if summary.inspection.allocation_id != allocation_id
            || summary.attestation.allocation_id != allocation_id
            || summary.inspection.controller_id != controller_id
            || summary.attestation.controller_id != controller_id
            || summary.inspection.pool_id != summary.attestation.pool_id
            || summary.inspection.runner_name != summary.attestation.runner_name
            || summary.inspection.scope_digest != summary.attestation.scope_digest
        {
            return Err(CleanupError::Scope);
        }
        summary.inspection.historical_audit = true;
        return Ok(summary.inspection);
    }

    let scope = journal
        .cleanup_resolution_scope(allocation_id)?
        .ok_or(CleanupError::Scope)?;
    if scope.allocation.controller_id != controller_id {
        return Err(CleanupError::Scope);
    }
    inspect_scope(&scope)
}

/// Resolve one current cleanup scope from a fresh, operator-supplied attestation.
///
/// # Errors
///
/// Returns an error when the attestation is invalid, stale, conflicting, or the
/// journal cannot commit the terminal transition.
pub fn resolve(
    journal: &Journal,
    attestation: &CleanupAttestation,
    now: u64,
) -> Result<bool, CleanupError> {
    validate_host()?;
    let canonical = serde_json::to_vec(attestation).map_err(|_| CleanupError::Serialization)?;
    let attestation_digest = hex::encode(Sha256::digest(canonical));
    if journal.cleanup_resolution_matches(&attestation.allocation_id, &attestation_digest)? {
        return Ok(false);
    }
    validate_attestation(attestation, now)?;
    let scope = journal
        .cleanup_resolution_scope(&attestation.allocation_id)?
        .ok_or(CleanupError::Scope)?;
    if scope.allocation.state == "deleted" {
        return Err(CleanupError::Conflict);
    }
    let inspection = inspect_scope(&scope)?;
    if attestation.controller_id != inspection.controller_id
        || attestation.pool_id != inspection.pool_id
        || attestation.runner_name != inspection.runner_name
        || attestation.scope_digest != inspection.scope_digest
        || attestation.verified_at_unix_seconds < scope_latest_operation_time(&scope)
    {
        return Err(CleanupError::Attestation);
    }
    let (uid, gid) = effective_identity()?;
    let audit = CleanupResolutionAudit {
        attestation_digest,
        evidence_reference: attestation.evidence_reference.clone(),
        verified_at: attestation.verified_at_unix_seconds,
        uid,
        gid,
    };
    let summary_json = serde_json::to_string(&CleanupResolutionSummary {
        attestation: attestation.clone(),
        inspection,
    })
    .map_err(|_| CleanupError::Serialization)?;
    journal
        .commit_cleanup_resolution(
            &attestation.allocation_id,
            &attestation.scope_digest,
            &audit,
            &summary_json,
        )
        .map_err(|error| match error {
            crate::journal::JournalError::Sqlite(rusqlite::Error::InvalidQuery) => {
                CleanupError::Attestation
            }
            other => CleanupError::Journal(other),
        })
}

fn inspect_scope(scope: &CleanupResolutionScope) -> Result<CleanupInspection, CleanupError> {
    let allocation = &scope.allocation;
    let dispatched_at = allocation.dispatched_at.ok_or(CleanupError::Scope)?;
    let catalog_fingerprint = allocation
        .catalog_fingerprint
        .as_ref()
        .filter(|value| !value.is_empty())
        .ok_or(CleanupError::Scope)?;
    let selected_offer = allocation
        .selected_offer_json
        .as_deref()
        .ok_or(CleanupError::Scope)?;
    let offer: CatalogOffer =
        serde_json::from_str(selected_offer).map_err(|_| CleanupError::Scope)?;
    let binding_json = allocation
        .binding_json
        .as_deref()
        .ok_or(CleanupError::Scope)?;
    let binding: ProviderBinding =
        serde_json::from_str(binding_json).map_err(|_| CleanupError::Scope)?;
    let snapshot: SnapshotDocument =
        serde_json::from_str(&scope.snapshot_json).map_err(|_| CleanupError::Scope)?;
    let provider_binding = snapshot
        .central
        .provider_bindings
        .as_ref()
        .and_then(|bindings| bindings.iter().find(|candidate| candidate.id == binding.id))
        .ok_or(CleanupError::Scope)?;
    let candidate = &offer.candidate;
    let selection = &offer.selection;
    if selection.binding_id != binding.id
        || selection.source_revision != binding.source_revision
        || selection.binary_sha256 != binding.executable_sha256
        || selection.configuration_sha256 != binding.configuration_sha256
        || provider_binding != &binding
        || candidate.cloud != binding.cloud
        || provider_binding.id != selection.binding_id
    {
        return Err(CleanupError::Scope);
    }
    let native_scope = snapshot_catalog_scope(&snapshot.central, catalog_fingerprint, &offer)
        .ok_or(CleanupError::Scope)?;
    let canonical_account = crate::allocation::canonical_native_account_scope(
        &native_scope.cloud,
        &native_scope.account_id,
    )
    .map_err(|_| CleanupError::Scope)?;
    if allocation.account_id.as_deref() != Some(canonical_account.as_str()) {
        return Err(CleanupError::Scope);
    }
    let latest_dispatched_attempt = scope
        .child_attempts
        .iter()
        .rev()
        .find(|attempt| attempt.dispatched_at.is_some())
        .ok_or(CleanupError::Scope)?;
    if latest_dispatched_attempt.offer_json != selected_offer
        || latest_dispatched_attempt.binding_json != binding_json
    {
        return Err(CleanupError::Scope);
    }
    let native_identity = allocation
        .native_identity_json
        .as_deref()
        .map(serde_json::from_str::<crate::cloud::PlacementIdentity>)
        .transpose()
        .map_err(|_| CleanupError::Scope)?;
    if native_identity.as_ref().is_some_and(|identity| {
        let account_or_location_mismatch = if native_scope.cloud == "azure" {
            !identity
                .account_id
                .eq_ignore_ascii_case(&native_scope.account_id)
                || !identity
                    .location
                    .eq_ignore_ascii_case(&native_scope.location)
        } else {
            identity.account_id != native_scope.account_id
                || identity.location != native_scope.location
        };
        identity.controller_id != allocation.controller_id
            || identity.cloud != native_scope.cloud
            || account_or_location_mismatch
    }) {
        return Err(CleanupError::Scope);
    }
    let cleanup_action = scope
        .actions
        .iter()
        .rev()
        .find(|action| {
            action.command == "DeleteInstance" && action.action_id > 0 && action.reusable
        })
        .ok_or(CleanupError::Scope)?;
    let unresolved_actions = scope
        .actions
        .iter()
        .filter(|action| matches!(action.state.as_str(), "pending" | "intent" | "ambiguous"))
        .map(|action| CleanupActionSummary {
            action_id: action.action_id,
            command: action.command.clone(),
            state: action.state.clone(),
            started_at: action.started_at,
            unresolved_create: action.unresolved_create,
            reusable: action.reusable,
        })
        .collect();
    let inspection = CleanupInspection {
        schema_version: 1,
        allocation_id: allocation.allocation_id.clone(),
        controller_id: allocation.controller_id.clone(),
        pool_id: allocation.pool_id.clone(),
        runner_name: allocation.runner_name.clone(),
        snapshot_id: allocation.snapshot_id.clone(),
        catalog_id: native_scope.catalog_id,
        catalog_fingerprint: catalog_fingerprint.clone(),
        cloud: candidate.cloud.clone(),
        logical_account_id: candidate.account.clone(),
        logical_location: candidate.location.clone(),
        native_account_id: native_scope.account_id,
        native_location: native_scope.location,
        launch_scope: native_scope.launch_scope,
        provider_binding_id: binding.id,
        provider_source_revision: binding.source_revision,
        provider_binary_sha256: binding.executable_sha256,
        provider_configuration_sha256: binding.configuration_sha256,
        native_resource_id: native_identity
            .as_ref()
            .map(|identity| identity.resource_id.clone()),
        native_host_id: native_identity
            .as_ref()
            .and_then(|identity| identity.host_id.clone()),
        native_dependencies: native_identity
            .as_ref()
            .map(|identity| identity.dependency_ids.clone())
            .unwrap_or_default(),
        native_identity_known: native_identity.is_some(),
        allocation_state: allocation.state.clone(),
        tombstone: allocation.tombstone,
        created_at: allocation.created_at,
        dispatched_at,
        returned_at: allocation.returned_at,
        cleanup_action_id: cleanup_action.action_id,
        cleanup_action_state: cleanup_action.state.clone(),
        cleanup_attempt_count: cleanup_action.attempt_count,
        cleanup_last_attempt_at: cleanup_action.last_attempt_at,
        cleanup_next_attempt_at: cleanup_action.next_attempt_at,
        unresolved_actions,
        scope_digest: scope_digest(scope)?,
        historical_audit: false,
    };
    Ok(inspection)
}

/// Hash the canonical retained resolution scope.
///
/// # Errors
///
/// Returns an error if the retained scope cannot be serialized.
pub fn scope_digest(scope: &CleanupResolutionScope) -> Result<String, CleanupError> {
    let canonical = serde_json::to_vec(scope).map_err(|_| CleanupError::Serialization)?;
    Ok(hex::encode(Sha256::digest(canonical)))
}

fn validate_attestation(attestation: &CleanupAttestation, now: u64) -> Result<(), CleanupError> {
    let reference = attestation.evidence_reference.as_bytes();
    if attestation.schema_version != 1
        || !attestation.acknowledgements.all_cloud_operations_terminal
        || !attestation
            .acknowledgements
            .no_local_provider_process_can_submit_work
        || !attestation
            .acknowledgements
            .all_resources_and_dependencies_proved_absent
        || reference.is_empty()
        || reference.len() > EVIDENCE_REFERENCE_MAX_BYTES
        || reference.first() == Some(&b'/')
        || !reference
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:/#-".contains(byte))
        || attestation.verified_at_unix_seconds > now
        || now.saturating_sub(attestation.verified_at_unix_seconds) > ATTESTATION_MAX_AGE_SECONDS
    {
        return Err(CleanupError::Attestation);
    }
    Ok(())
}

fn scope_latest_operation_time(scope: &CleanupResolutionScope) -> u64 {
    scope
        .allocation
        .dispatched_at
        .into_iter()
        .chain(scope.allocation.returned_at)
        .chain(scope.child_attempts.iter().flat_map(|attempt| {
            attempt
                .dispatched_at
                .into_iter()
                .chain(attempt.completed_at)
        }))
        .chain(scope.actions.iter().flat_map(|action| {
            std::iter::once(action.started_at)
                .chain(action.completed_at)
                .chain(action.last_attempt_at)
        }))
        .max()
        .unwrap_or(0)
}

fn snapshot_catalog_scope(
    config: &ConfigPatch,
    expected_fingerprint: &str,
    offer: &CatalogOffer,
) -> Option<RetainedNativeScope> {
    let candidate = &offer.candidate;
    let mut matching: Option<(&str, &str)> = None;
    for (cloud, catalog_ids) in [
        (
            "aws",
            config
                .aws_catalog_bindings
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|catalog| catalog.id.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "azure",
            config
                .azure_catalog_bindings
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|catalog| catalog.id.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "gcp",
            config
                .gcp_catalog_bindings
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|catalog| catalog.id.as_str())
                .collect::<Vec<_>>(),
        ),
    ] {
        for catalog_id in catalog_ids {
            if crate::allocation::catalog_configuration_fingerprint(config, cloud, catalog_id)
                .is_ok_and(|fingerprint| fingerprint == expected_fingerprint)
            {
                if matching.is_some() {
                    return None;
                }
                matching = Some((cloud, catalog_id));
            }
        }
    }
    let (cloud, catalog_id) = matching?;
    match cloud {
        "aws" => {
            let catalog = config
                .aws_catalog_bindings
                .as_deref()?
                .iter()
                .find(|catalog| catalog.id == catalog_id)?;
            let network = catalog.locations.get(&candidate.location)?;
            let image = catalog.images.get(&candidate.image)?;
            let ProviderLaunchFacts::Aws(facts) = &offer.launch_facts else {
                return None;
            };
            (candidate.cloud == "aws"
                && catalog.provider_binding_id == offer.selection.binding_id
                && candidate.account == catalog.account_id
                && offer.selection.image == image.image_id
                && facts.availability_zone == network.availability_zone
                && facts.subnet_id == network.subnet_id)
                .then(|| RetainedNativeScope {
                    catalog_id: catalog.id.clone(),
                    cloud: cloud.to_owned(),
                    account_id: catalog.account_id.clone(),
                    location: catalog.region.clone(),
                    launch_scope: CleanupLaunchScope {
                        image_alias: candidate.image.clone(),
                        native_image_id: image.image_id.clone(),
                        availability_zone: Some(facts.availability_zone.clone()),
                        subnet_id: Some(facts.subnet_id.clone()),
                        vnet_subnet_id: None,
                        network_id: None,
                        subnetwork_id: None,
                    },
                })
        }
        "azure" => {
            let catalog = config
                .azure_catalog_bindings
                .as_deref()?
                .iter()
                .find(|catalog| catalog.id == catalog_id)?;
            let network = catalog.locations.get(&candidate.location)?;
            let image = catalog.images.get(&candidate.image)?;
            let ProviderLaunchFacts::Azure(facts) = &offer.launch_facts else {
                return None;
            };
            (candidate.cloud == "azure"
                && catalog.provider_binding_id == offer.selection.binding_id
                && candidate.account == catalog.account_id
                && offer.selection.image == image.image_id
                && facts.subscription_id == catalog.subscription_id
                && facts.location.eq_ignore_ascii_case(&network.location))
            .then(|| RetainedNativeScope {
                catalog_id: catalog.id.clone(),
                cloud: cloud.to_owned(),
                account_id: catalog.subscription_id.clone(),
                location: facts.location.clone(),
                launch_scope: CleanupLaunchScope {
                    image_alias: candidate.image.clone(),
                    native_image_id: image.image_id.clone(),
                    availability_zone: None,
                    subnet_id: None,
                    vnet_subnet_id: network.vnet_subnet_id.clone(),
                    network_id: None,
                    subnetwork_id: None,
                },
            })
        }
        "gcp" => {
            let catalog = config
                .gcp_catalog_bindings
                .as_deref()?
                .iter()
                .find(|catalog| catalog.id == catalog_id)?;
            let image = catalog.images.get(&candidate.image)?;
            let ProviderLaunchFacts::Gcp(facts) = &offer.launch_facts else {
                return None;
            };
            let (configured_project, configured_image) =
                crate::cloud::gcp::image_path_parts(&catalog.project_id, &image.image_id).ok()?;
            let (native_project, native_image) =
                crate::cloud::gcp::image_path_parts(&catalog.project_id, &facts.image_id).ok()?;
            (candidate.cloud == "gcp"
                && catalog.provider_binding_id == offer.selection.binding_id
                && candidate.account == catalog.project_id
                && candidate.location == catalog.pricing_location
                && facts.project_id == catalog.project_id
                && facts.zone == catalog.zone
                && offer.selection.image == image.image_id
                && configured_project == native_project
                && configured_image == native_image)
                .then(|| RetainedNativeScope {
                    catalog_id: catalog.id.clone(),
                    cloud: cloud.to_owned(),
                    account_id: catalog.project_id.clone(),
                    location: catalog.zone.clone(),
                    launch_scope: CleanupLaunchScope {
                        image_alias: candidate.image.clone(),
                        native_image_id: facts.image_id.clone(),
                        availability_zone: None,
                        subnet_id: None,
                        vnet_subnet_id: None,
                        network_id: Some(facts.network_id.clone()),
                        subnetwork_id: Some(facts.subnetwork_id.clone()),
                    },
                })
        }
        _ => None,
    }
}

#[cfg(unix)]
fn effective_identity() -> Result<(u32, u32), CleanupError> {
    // SAFETY: geteuid has no pointer arguments or preconditions.
    let uid = unsafe { libc::geteuid() };
    // SAFETY: getegid has no pointer arguments or preconditions.
    let gid = unsafe { libc::getegid() };
    Ok((uid, gid))
}

#[cfg(not(unix))]
fn effective_identity() -> Result<(u32, u32), CleanupError> {
    Err(CleanupError::UnsupportedHost)
}

/// Ensure offline cleanup is running on a supported Unix host.
///
/// # Errors
///
/// Returns [`CleanupError::UnsupportedHost`] on non-Unix platforms.
pub fn validate_host() -> Result<(), CleanupError> {
    #[cfg(unix)]
    {
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Err(CleanupError::UnsupportedHost)
    }
}
