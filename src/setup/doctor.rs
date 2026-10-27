//! Static checks and explicit live checks report their separate qualification scopes.

use super::{CentralConfig, ConfigurationFile, SetupError, adoption};
use clap::Args;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Args)]
pub struct DoctorOptions {
    #[arg(long, default_value = "/etc/outrider/config.toml")]
    pub config: PathBuf,
    #[arg(long, default_value = "default")]
    pub profile: String,
    /// Verify existing API identities and ask the running daemon for live-priced selection.
    #[arg(long)]
    pub online: bool,
}

#[derive(Serialize)]
pub struct DoctorReport {
    pub configuration_valid: bool,
    pub native_providers: usize,
    pub api_identities_verified: bool,
    pub eligible_live_candidates: Option<usize>,
    pub runner_execution_verified: bool,
    pub notes: Vec<&'static str>,
}

/// Check installation without allocating runners or changing existing GARM entities.
///
/// # Errors
/// Returns a fixed error for static validation, API identity or live selection failure.
pub async fn doctor(options: &DoctorOptions) -> Result<DoctorReport, SetupError> {
    let file = ConfigurationFile::open(&options.config)?;
    let source = CentralConfig::from_toml(file.text()?).map_err(|_| SetupError::Configuration)?;
    let count = source.providers.len();
    if count == 0 {
        return Err(SetupError::Configuration);
    }

    let config = source
        .load_native_configuration(&std::env::vars_os().collect())
        .map_err(|_| SetupError::Configuration)?;
    config
        .normalize_profile(None, &options.profile, None)
        .map_err(|_| SetupError::Configuration)?;
    let mut report = DoctorReport {
        configuration_valid: true,
        native_providers: count,
        api_identities_verified: false,
        eligible_live_candidates: None,
        runner_execution_verified: false,
        notes: vec![
            "Hidden App-key/webhook-secret agreement and real guest execution require a signed live trial.",
            "Check GitHub App Contents read and Checks write; read-only API checks cannot prove all write permissions.",
        ],
    };
    if options.online {
        let status = crate::daemon::configuration_status_remote(
            config.control.as_ref().ok_or(SetupError::Configuration)?,
            config.ingress.as_ref().ok_or(SetupError::Configuration)?,
        )
        .await
        .map_err(|_| SetupError::LiveSelection)?;
        let expected = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(file.bytes()));
        if status.configuration_sha256 != expected {
            return Err(SetupError::StaleDaemon);
        }

        adoption::resolve_identities(config.ingress.as_ref().ok_or(SetupError::Configuration)?)
            .await?;
        let explanation = crate::daemon::explain_remote(
            config.control.as_ref().ok_or(SetupError::Configuration)?,
            config.ingress.as_ref().ok_or(SetupError::Configuration)?,
            crate::allocation::ExplainRequest {
                profile: Some(options.profile.clone()),
                snapshot_id: None,
            },
        )
        .await
        .map_err(|_| SetupError::LiveSelection)?;
        let eligible = explanation
            .candidates
            .iter()
            .filter(|candidate| candidate.eligible)
            .count();
        if explanation.policy_error.is_some() || explanation.quote_missing || eligible == 0 {
            return Err(SetupError::LiveSelection);
        }

        report.api_identities_verified = true;
        report.eligible_live_candidates = Some(eligible);
    } else {
        report.notes.push("Static checks only; start the daemon and run doctor --online for live API/price checks.");
    }

    Ok(report)
}
