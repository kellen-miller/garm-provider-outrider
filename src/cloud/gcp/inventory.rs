use std::collections::BTreeSet;

use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use tokio::time::Instant;

use super::connection::path_segment;
use super::{CloudError, GcpBackend, failure};
use crate::cloud::inventory::{
    CloudInventory, InventoryCollection, InventoryKind, InventoryOwnership, InventoryResource,
    InventoryScope,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InventoryPage {
    kind: String,
    #[serde(default)]
    items: Vec<Value>,
    next_page_token: Option<String>,
    warning: Option<InventoryWarning>,
}

#[derive(Deserialize)]
struct InventoryWarning {
    code: String,
}

#[async_trait::async_trait]
impl CloudInventory for GcpBackend {
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

        let region = self
            .settings
            .zone
            .rsplit_once('-')
            .filter(|(region, zone)| !region.is_empty() && !zone.is_empty())
            .map(|(region, _)| region.to_owned())
            .ok_or_else(|| failure("configuration", "GCP inventory zone lacks a region"))?;
        let (location_kind, location, resource_kind, page_kind) = match kind {
            InventoryKind::VirtualMachine => (
                "zones",
                &self.settings.zone,
                "instances",
                "compute#instanceList",
            ),
            InventoryKind::Disk => ("zones", &self.settings.zone, "disks", "compute#diskList"),
            InventoryKind::AllocatedAddress => {
                ("regions", &region, "addresses", "compute#addressList")
            }
        };
        let project = path_segment(&self.settings.project_id)?;
        let location_segment = path_segment(location)?;
        let root = format!("projects/{project}/{location_kind}/{location_segment}/{resource_kind}");
        let scope = InventoryScope {
            cloud: "gcp".to_owned(),
            account: self.settings.project_id.clone(),
            location: location.clone(),
        };
        let mut resources = Vec::new();
        let mut tokens = BTreeSet::new();
        let mut native_ids = BTreeSet::new();
        let mut next_token: Option<String> = None;
        loop {
            let mut path = format!("/compute/v1/{root}?maxResults=500");
            if let Some(token) = &next_token {
                path.push_str("&pageToken=");
                path.push_str(&urlencoding::encode(token));
            }

            let (page, _): (InventoryPage, _) = self
                .request_json(&self.settings.compute_endpoint, &path, deadline)
                .await?;
            if page.kind != page_kind
                || page.warning.as_ref().is_some_and(|warning| {
                    warning.code != "NO_RESULTS_ON_PAGE" || !page.items.is_empty()
                })
            {
                return Err(failure(
                    "protocol",
                    "GCP inventory page is incomplete or unexpected",
                ));
            }

            if resources.len().saturating_add(page.items.len()) > 10_000 {
                return Err(failure(
                    "inventory_limit",
                    "GCP inventory exceeds the resource limit",
                ));
            }

            for row in page.items {
                let resource = map_resource(row, kind, &scope, &root, controller_id)?;
                if !native_ids.insert(resource.id.clone()) {
                    return Err(failure("pagination", "GCP inventory repeats a resource"));
                }

                resources.push(resource);
            }

            let Some(token) = page.next_page_token.filter(|token| !token.is_empty()) else {
                break;
            };
            if !tokens.insert(token.clone()) || tokens.len() > 1000 {
                return Err(failure(
                    "pagination",
                    "GCP inventory pagination did not make progress",
                ));
            }

            next_token = Some(token);
        }

        Ok(InventoryCollection {
            scope,
            kind,
            resources,
        })
    }
}

fn map_resource(
    row: Value,
    kind: InventoryKind,
    scope: &InventoryScope,
    root: &str,
    controller_id: &str,
) -> Result<InventoryResource, CloudError> {
    let name = row["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| failure("protocol", "GCP inventory resource lacks a name"))?;
    let incarnation = row["id"]
        .as_str()
        .and_then(|id| id.parse::<u64>().ok())
        .or_else(|| row["id"].as_u64())
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            failure(
                "protocol",
                "GCP inventory resource lacks an exact native ID",
            )
        })?;
    let location_field = match kind {
        InventoryKind::VirtualMachine | InventoryKind::Disk => "zone",
        InventoryKind::AllocatedAddress => "region",
    };
    let location = row[location_field]
        .as_str()
        .ok_or_else(|| failure("protocol", "GCP inventory resource lacks its location"))?;
    let expected_location = root
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .ok_or_else(|| failure("configuration", "GCP inventory scope is invalid"))?;
    if compute_reference(location, &scope.account)? != expected_location {
        return Err(failure(
            "protocol",
            "GCP inventory resource has a different location",
        ));
    }

    let attached_to = match row.get("users") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(users)) => users
            .iter()
            .map(|user| {
                let reference = user.as_str().ok_or_else(|| {
                    failure(
                        "protocol",
                        "GCP inventory users must be resource references",
                    )
                })?;
                compute_reference(reference, &scope.account)
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(failure("protocol", "GCP inventory users must be a list")),
    };
    Ok(InventoryResource {
        id: format!("{root}/{}", path_segment(name)?),
        incarnation: Some(incarnation.to_string()),
        name: name.to_owned(),
        location: scope.location.clone(),
        ownership: InventoryOwnership::from_tag(
            row["labels"]["garmcontrollerid"].as_str(),
            controller_id,
        ),
        state: row["status"].as_str().map(str::to_owned),
        attached_to,
    })
}

fn compute_reference(value: &str, project: &str) -> Result<String, CloudError> {
    let url = Url::parse(value)
        .map_err(|_| failure("protocol", "GCP inventory reference is not a URL"))?;
    let path = url
        .path()
        .strip_prefix("/compute/v1/")
        .filter(|path| path.starts_with(&format!("projects/{project}/")))
        .ok_or_else(|| failure("protocol", "GCP inventory reference is outside its project"))?;
    if url.scheme() != "https"
        || url.host_str() != Some("www.googleapis.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(failure(
            "protocol",
            "GCP inventory reference has an unexpected origin",
        ));
    }

    Ok(path.to_owned())
}
