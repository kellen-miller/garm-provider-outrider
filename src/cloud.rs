use std::{collections::BTreeMap, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::policy::{CatalogOffer, PriceQuote};

pub mod aws;
pub mod azure;
pub mod gcp;

#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    #[error("{0} cloud adapter is not initialized")]
    Unavailable(&'static str),
    #[error("cloud operation failed: {0}")]
    Failure(CloudFailure),
    #[error("cloud pricing is unavailable: {0}")]
    PriceUnavailable(Box<CloudError>),
}

impl CloudError {
    pub(crate) fn price_unavailable(error: CloudError) -> Self {
        Self::PriceUnavailable(Box::new(error))
    }
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct CloudFailure {
    pub kind: String,
    pub message: String,
    pub partial_resources: Vec<PlacementIdentity>,
    pub provider_request_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlacementIdentity {
    pub controller_id: String,
    pub cloud: String,
    pub account_id: String,
    pub location: String,
    pub resource_id: String,
    pub host_id: Option<String>,
    pub dependency_ids: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProviderInstance {
    pub identity: PlacementIdentity,
    pub name: String,
    pub state: String,
    pub ownership_tags: Vec<(String, String)>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct InterruptionEvidence {
    pub source_api: String,
    pub api_version: String,
    /// Provider's original operation, request, or annotation identifier.
    pub source_id: String,
    /// Stable persistence key; Azure revisions include provider occurredTime.
    pub event_id: String,
    pub occurred_at: String,
    pub reason_fields_json: String,
}

#[derive(Clone, Debug)]
pub struct OwnershipScope {
    pub controller_id: String,
    pub resource_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CatalogQuery {
    pub clouds: Vec<String>,
    pub accounts: Vec<String>,
    pub locations: Vec<String>,
    pub profile: crate::config::RunnerProfile,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogDiscovery {
    pub offers: Vec<CatalogOffer>,
    pub exclusions: Vec<CatalogExclusion>,
    pub queried: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogExclusion {
    pub reason: String,
    pub observed_count: usize,
}

#[derive(Default)]
pub struct CatalogExclusions(BTreeMap<String, usize>);

impl CatalogExclusions {
    pub fn record(&mut self, reason: &str) {
        *self.0.entry(reason.to_owned()).or_default() += 1;
    }

    pub fn into_catalog_exclusions(self) -> Vec<CatalogExclusion> {
        self.0
            .into_iter()
            .map(|(reason, observed_count)| CatalogExclusion {
                reason,
                observed_count,
            })
            .collect()
    }
}

#[async_trait]
pub trait CloudCatalog: Send + Sync {
    async fn catalog(
        &self,
        query: CatalogQuery,
        deadline: Instant,
    ) -> Result<CatalogDiscovery, CloudError>;

    async fn quote(
        &self,
        offer: &CatalogOffer,
        duration: Duration,
        maximum_price_age: Duration,
        deadline: Instant,
    ) -> Result<PriceQuote, CloudError>;

    async fn inspect(
        &self,
        identity: &PlacementIdentity,
        deadline: Instant,
    ) -> Result<Option<ProviderInstance>, CloudError>;

    /// Enumerate owned identities; runtime state may be unknown.
    /// Use `inspect` when current runtime state is required.
    async fn list(
        &self,
        scope: &OwnershipScope,
        deadline: Instant,
    ) -> Result<Vec<ProviderInstance>, CloudError>;
}
