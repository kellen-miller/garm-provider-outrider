//! Read-only billable-resource discovery, separate from runner allocation.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::CloudError;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryKind {
    VirtualMachine,
    Disk,
    AllocatedAddress,
}

impl InventoryKind {
    pub const ALL: [Self; 3] = [Self::VirtualMachine, Self::Disk, Self::AllocatedAddress];
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryScope {
    pub cloud: String,
    pub account: String,
    /// Native region, zone, or subscription-wide scope; never a profile alias.
    pub location: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryOwnership {
    Controller,
    OtherController,
    Untagged,
}

impl InventoryOwnership {
    /// Classify ownership without retaining arbitrary, possibly sensitive tag values.
    pub fn from_tag(tag: Option<&str>, controller_id: &str) -> Self {
        match tag {
            Some(value) if value == controller_id => Self::Controller,
            Some(_) => Self::OtherController,
            None => Self::Untagged,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryResource {
    pub id: String,
    pub incarnation: Option<String>,
    pub name: String,
    pub location: String,
    pub ownership: InventoryOwnership,
    pub state: Option<String>,
    pub attached_to: Vec<String>,
    /// Native resources referenced by this resource; no credentials or metadata.
    pub dependencies: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryCollection {
    pub scope: InventoryScope,
    pub kind: InventoryKind,
    pub resources: Vec<InventoryResource>,
}

/// A bounded observation of connected catalog scopes, never an account-wide clean bill.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryReport {
    pub controller_id: String,
    pub started_at_unix_seconds: u64,
    pub finished_at_unix_seconds: u64,
    pub catalogs: Vec<CatalogInventory>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogInventory {
    pub catalog_id: String,
    pub collections: Vec<InventoryCollection>,
}

#[async_trait]
pub trait CloudInventory: Send + Sync {
    /// List every resource of this kind in the declared scope, including untagged ones.
    /// Errors or incomplete pagination must never become a successful empty collection.
    async fn inventory(
        &self,
        kind: InventoryKind,
        controller_id: &str,
        deadline: Instant,
    ) -> Result<InventoryCollection, CloudError>;
}
