//! Safe allocation-footprint projection owned by the product journal.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Journal, JournalError};
use crate::cloud::{PlacementIdentity, inventory::InventoryScope};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InventorySnapshot {
    pub active_allocations: Vec<InventoryActiveAllocation>,
    pub dispatch_fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryActiveAllocation {
    pub id: String,
    pub created_at: u64,
    pub identity: Option<InventoryVmIdentity>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryVmIdentity {
    pub scope: InventoryScope,
    pub resource_id: String,
    pub incarnation: Option<String>,
}

impl Journal {
    /// Read a bounded allocation-footprint snapshot and durable dispatch-history digest.
    /// Private binding, bootstrap and provider JSON never enter this projection.
    ///
    /// # Errors
    /// Returns an error on storage failure, unknown lifecycle state, invalid
    /// retained identity, or more than 10,000 cloud-active allocations.
    pub fn inventory_snapshot(&self, controller: &str) -> Result<InventorySnapshot, JournalError> {
        if controller.is_empty() {
            return Err(JournalError::InvalidInventory);
        }

        let connection = self.connection.lock().map_err(|_| JournalError::Poisoned)?;
        let transaction = connection.unchecked_transaction()?;
        let mut statement = transaction.prepare(
            "SELECT allocation_id,created_at,native_identity_json,state,dispatched_at FROM allocations WHERE controller_id=?1 AND state!='deleted' AND (state NOT IN ('reserved','selecting','no_candidate') OR dispatched_at IS NOT NULL OR native_identity_json IS NOT NULL) ORDER BY allocation_id LIMIT 10001"
        )?;
        let mut rows = statement.query([controller])?;
        let mut active_allocations = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let created_at =
                u64::try_from(row.get::<_, i64>(1)?).map_err(|_| JournalError::InvalidInventory)?;
            let json: Option<String> = row.get(2)?;
            let state: String = row.get(3)?;
            let dispatched_at: Option<i64> = row.get(4)?;
            if id.is_empty()
                || dispatched_at.is_some_and(|at| at < 0)
                || !matches!(
                    state.as_str(),
                    "reserved"
                        | "selecting"
                        | "no_candidate"
                        | "dispatched"
                        | "ambiguous"
                        | "created"
                        | "deleting"
                )
                || active_allocations.len() >= 10_000
            {
                return Err(JournalError::InvalidInventory);
            }

            let identity = json
                .as_deref()
                .map(|json| project_identity(json, controller))
                .transpose()?;
            active_allocations.push(InventoryActiveAllocation {
                id,
                created_at,
                identity,
            });
        }

        drop(rows);
        drop(statement);
        let mut statement = transaction.prepare("SELECT allocation_id,dispatched_at FROM allocations WHERE controller_id=?1 AND dispatched_at IS NOT NULL ORDER BY allocation_id")?;
        let mut rows = statement.query([controller])?;
        let mut digest = Sha256::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let at: i64 = row.get(1)?;
            if at < 0 {
                return Err(JournalError::InvalidInventory);
            }

            digest.update(id.len().to_le_bytes());
            digest.update(id.as_bytes());
            digest.update(at.to_le_bytes());
        }

        Ok(InventorySnapshot {
            active_allocations,
            dispatch_fingerprint: hex::encode(digest.finalize()),
        })
    }
}

fn project_identity(json: &str, controller: &str) -> Result<InventoryVmIdentity, JournalError> {
    if json.len() > 128 * 1024 {
        return Err(JournalError::InvalidInventory);
    }

    let identity: PlacementIdentity =
        serde_json::from_str(json).map_err(|_| JournalError::InvalidInventory)?;
    if identity.controller_id != controller
        || identity.account_id.is_empty()
        || identity.location.is_empty()
        || identity.resource_id.is_empty()
    {
        return Err(JournalError::InvalidInventory);
    }

    let (location, resource_id, incarnation) = match identity.cloud.as_str() {
        "aws" => (
            identity.location.clone(),
            identity.resource_id.clone(),
            None,
        ),
        "azure" => {
            let id = identity
                .dependency_ids
                .get("vm_id")
                .filter(|id| !id.is_empty())
                .ok_or(JournalError::InvalidInventory)?;
            (
                "subscription".to_owned(),
                identity.resource_id.clone(),
                Some(id.clone()),
            )
        }
        "gcp" => {
            let name = identity
                .dependency_ids
                .get("instance_name")
                .filter(|name| !name.is_empty() && !name.contains('/'))
                .ok_or(JournalError::InvalidInventory)?;
            let id = identity
                .resource_id
                .parse::<u64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or(JournalError::InvalidInventory)?;
            (
                identity.location.clone(),
                format!(
                    "projects/{}/zones/{}/instances/{name}",
                    identity.account_id, identity.location
                ),
                Some(id.to_string()),
            )
        }
        _ => return Err(JournalError::InvalidInventory),
    };
    Ok(InventoryVmIdentity {
        scope: InventoryScope {
            cloud: identity.cloud,
            account: identity.account_id,
            location,
        },
        resource_id,
        incarnation,
    })
}
