use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::Value;
use tokio::time::Instant;

use super::{AzureBackend, CloudError, failure};
use crate::cloud::inventory::{
    CloudInventory, InventoryCollection, InventoryKind, InventoryOwnership, InventoryResource,
    InventoryScope,
};

#[derive(Deserialize)]
struct InventoryPage {
    value: Vec<Value>,
    #[serde(rename = "nextLink")]
    next_link: Option<String>,
}

#[async_trait::async_trait]
impl CloudInventory for AzureBackend {
    async fn inventory(
        &self,
        kind: InventoryKind,
        controller_id: &str,
        deadline: Instant,
    ) -> Result<InventoryCollection, CloudError> {
        if controller_id.trim().is_empty() {
            return Err(failure(
                "configuration",
                "inventory requires a controller identity",
            ));
        }

        let (provider, resource_type, api) = match kind {
            InventoryKind::VirtualMachine => {
                ("Microsoft.Compute", "virtualMachines", super::COMPUTE_API)
            }
            InventoryKind::Disk => ("Microsoft.Compute", "disks", "2025-01-02"),
            InventoryKind::AllocatedAddress => {
                ("Microsoft.Network", "publicIPAddresses", "2024-05-01")
            }
        };
        let path = format!(
            "/subscriptions/{}/providers/{provider}/{resource_type}",
            self.settings.subscription_id
        );
        let mut next = Some(format!(
            "{}{path}?api-version={api}",
            self.settings.management_endpoint.trim_end_matches('/')
        ));
        let mut visited = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut resources = Vec::new();
        while let Some(url) = next {
            if !visited.insert(url.clone()) || visited.len() > 1000 {
                return Err(failure(
                    "pagination",
                    "Azure inventory pagination did not make progress",
                ));
            }

            let page: InventoryPage =
                serde_json::from_value(self.get_management(&url, &path, deadline).await?)
                    .map_err(|_| failure("protocol", "Azure inventory page is invalid"))?;
            if resources.len().saturating_add(page.value.len()) > 10_000 {
                return Err(failure(
                    "inventory_limit",
                    "Azure inventory exceeds the resource limit",
                ));
            }

            for row in page.value {
                let resource = map_resource(
                    row,
                    kind,
                    &self.settings.subscription_id,
                    controller_id,
                    provider,
                    resource_type,
                )?;
                if !ids.insert(resource.id.to_ascii_lowercase()) {
                    return Err(failure("pagination", "Azure inventory repeats a resource"));
                }

                resources.push(resource);
            }

            next = page.next_link.filter(|link| !link.is_empty());
        }

        Ok(InventoryCollection {
            scope: InventoryScope {
                cloud: "azure".to_owned(),
                account: self.settings.subscription_id.clone(),
                location: "subscription".to_owned(),
            },
            kind,
            resources,
        })
    }
}

fn map_resource(
    row: Value,
    kind: InventoryKind,
    subscription: &str,
    controller_id: &str,
    provider: &str,
    resource_type: &str,
) -> Result<InventoryResource, CloudError> {
    let id = row["id"]
        .as_str()
        .ok_or_else(|| failure("protocol", "Azure inventory lacks a native ID"))?;
    let parts = resource_id(id, subscription)?;
    let name = row["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| failure("protocol", "Azure inventory lacks a name"))?;
    if parts.len() != 9
        || !parts[6].eq_ignore_ascii_case(provider)
        || !parts[7].eq_ignore_ascii_case(resource_type)
        || !parts[8].eq_ignore_ascii_case(name)
    {
        return Err(failure(
            "protocol",
            "Azure inventory resource has a different type or name",
        ));
    }

    let location = row["location"]
        .as_str()
        .filter(|location| !location.is_empty())
        .ok_or_else(|| failure("protocol", "Azure inventory lacks a location"))?;
    let incarnation_field = match kind {
        InventoryKind::VirtualMachine => Some("vmId"),
        InventoryKind::Disk => Some("uniqueId"),
        InventoryKind::AllocatedAddress => Some("resourceGuid"),
    };
    let incarnation = incarnation_field
        .map(|field| {
            row["properties"][field]
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| failure("protocol", "Azure inventory lacks its incarnation ID"))
        })
        .transpose()?;
    let mut attached_to = Vec::new();
    if kind == InventoryKind::Disk {
        if let Some(value) = row.get("managedBy").filter(|value| !value.is_null()) {
            let reference = value
                .as_str()
                .ok_or_else(|| failure("protocol", "Azure disk attachment is invalid"))?;
            resource_id(reference, subscription)?;
            attached_to.push(reference.to_owned());
        }

        if let Some(value) = row
            .get("managedByExtended")
            .filter(|value| !value.is_null())
        {
            for value in value
                .as_array()
                .ok_or_else(|| failure("protocol", "Azure shared disk attachments are invalid"))?
            {
                let reference = value.as_str().ok_or_else(|| {
                    failure("protocol", "Azure shared disk attachment is invalid")
                })?;
                resource_id(reference, subscription)?;
                attached_to.push(reference.to_owned());
            }
        }
    }

    if kind == InventoryKind::AllocatedAddress
        && let Some(value) = row["properties"]
            .get("ipConfiguration")
            .filter(|value| !value.is_null())
    {
        let reference = value["id"]
            .as_str()
            .ok_or_else(|| failure("protocol", "Azure address attachment is invalid"))?;
        resource_id(reference, subscription)?;
        attached_to.push(reference.to_owned());
        let parts = resource_id(reference, subscription)?;
        if parts.len() == 11
            && parts[6].eq_ignore_ascii_case("Microsoft.Network")
            && parts[7].eq_ignore_ascii_case("networkInterfaces")
            && parts[9].eq_ignore_ascii_case("ipConfigurations")
        {
            attached_to.push(parts[..9].join("/"));
        }
    }

    attached_to.sort();
    attached_to.dedup();
    let mut owner = None;
    if let Some(tags) = row.get("tags").filter(|tags| !tags.is_null()) {
        for (key, value) in tags
            .as_object()
            .ok_or_else(|| failure("protocol", "Azure inventory tags are invalid"))?
        {
            if key.eq_ignore_ascii_case("garm-controller-id") {
                if owner.is_some() {
                    return Err(failure(
                        "protocol",
                        "Azure inventory repeats its ownership tag",
                    ));
                }

                owner = Some(value.as_str().ok_or_else(|| {
                    failure("protocol", "Azure inventory ownership tag is invalid")
                })?);
            }
        }
    }

    Ok(InventoryResource {
        id: id.to_owned(),
        incarnation,
        name: name.to_owned(),
        location: location.to_owned(),
        ownership: InventoryOwnership::from_tag(owner, controller_id),
        // ProvisioningState is not VM power state. Inventory never infers running/stopped.
        state: if kind == InventoryKind::VirtualMachine {
            None
        } else {
            row["properties"]["provisioningState"]
                .as_str()
                .map(str::to_owned)
        },
        attached_to,
        dependencies: if kind == InventoryKind::VirtualMachine {
            vm_dependencies(&row, subscription)?
        } else {
            Vec::new()
        },
    })
}

fn vm_dependencies(row: &Value, subscription: &str) -> Result<Vec<String>, CloudError> {
    let properties = &row["properties"];
    let mut references = Vec::new();
    if let Some(disk) = properties["storageProfile"].get("osDisk")
        && let Some(id) = disk["managedDisk"].get("id")
    {
        references.push(id);
    }

    if let Some(disks) = properties["storageProfile"].get("dataDisks") {
        for disk in disks
            .as_array()
            .ok_or_else(|| failure("protocol", "Azure data disk references are invalid"))?
        {
            if let Some(id) = disk["managedDisk"].get("id") {
                references.push(id);
            }
        }
    }

    if let Some(interfaces) = properties["networkProfile"].get("networkInterfaces") {
        for interface in interfaces
            .as_array()
            .ok_or_else(|| failure("protocol", "Azure network references are invalid"))?
        {
            references.push(&interface["id"]);
        }
    }

    references
        .into_iter()
        .map(|value| {
            let reference = value.as_str().ok_or_else(|| {
                failure("protocol", "Azure VM dependency lacks a resource reference")
            })?;
            resource_id(reference, subscription)?;
            Ok(reference.to_owned())
        })
        .collect()
}

fn resource_id<'a>(id: &'a str, subscription: &str) -> Result<Vec<&'a str>, CloudError> {
    let parts: Vec<_> = id.split('/').collect();
    if parts.len() < 9
        || !parts[0].is_empty()
        || !parts[1].eq_ignore_ascii_case("subscriptions")
        || !parts[2].eq_ignore_ascii_case(subscription)
        || !parts[3].eq_ignore_ascii_case("resourceGroups")
        || parts[4].is_empty()
        || !parts[5].eq_ignore_ascii_case("providers")
        || parts[6..].iter().any(|part| {
            part.is_empty() || matches!(*part, "." | "..") || part.contains(['?', '#', '%'])
        })
    {
        return Err(failure(
            "protocol",
            "Azure inventory reference is outside its subscription",
        ));
    }

    Ok(parts)
}
