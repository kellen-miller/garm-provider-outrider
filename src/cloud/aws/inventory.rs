use std::collections::BTreeSet;

use aws_sdk_ec2::types::Tag;
use tokio::time::{Instant, timeout_at};

use super::{AwsBackend, CloudError, failure, sdk_failure};
use crate::cloud::inventory::{
    CloudInventory, InventoryCollection, InventoryKind, InventoryOwnership, InventoryResource,
    InventoryScope,
};

#[async_trait::async_trait]
impl CloudInventory for AwsBackend {
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
                vec![],
            ));
        }

        let mut resources = Vec::new();
        let mut tokens = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut next_token = None;
        loop {
            let (page, token) = match kind {
                InventoryKind::VirtualMachine => {
                    let output = timeout_at(
                        deadline,
                        Box::pin(
                            self.ec2
                                .describe_instances()
                                .max_results(500)
                                .set_next_token(next_token)
                                .send(),
                        ),
                    )
                    .await
                    .map_err(|_| failure("timeout", "AWS inventory timed out", vec![]))?
                    .map_err(|error| sdk_failure(&error, "AWS VM inventory failed", vec![]))?;
                    let mut page = Vec::new();
                    for reservation in output.reservations() {
                        if reservation.owner_id() != Some(self.settings.account_id.as_str()) {
                            return Err(failure(
                                "protocol",
                                "AWS inventory reservation has a different account",
                                vec![],
                            ));
                        }

                        for instance in reservation.instances() {
                            let id = required_id(instance.instance_id(), "i-")?;
                            let dependencies = instance
                                .block_device_mappings()
                                .iter()
                                .filter_map(|mapping| mapping.ebs())
                                .map(|disk| {
                                    required_id(disk.volume_id(), "vol-").map(str::to_owned)
                                })
                                .chain(instance.network_interfaces().iter().map(|interface| {
                                    required_id(interface.network_interface_id(), "eni-")
                                        .map(str::to_owned)
                                }))
                                .collect::<Result<Vec<_>, _>>()?;
                            page.push(InventoryResource {
                                id: id.to_owned(),
                                incarnation: None,
                                name: id.to_owned(),
                                location: instance
                                    .placement()
                                    .and_then(|placement| placement.availability_zone())
                                    .unwrap_or(&self.settings.region)
                                    .to_owned(),
                                ownership: ownership(instance.tags(), controller_id),
                                state: instance
                                    .state()
                                    .and_then(|state| state.name())
                                    .map(|state| state.as_str().to_owned()),
                                attached_to: Vec::new(),
                                dependencies,
                            });
                        }
                    }

                    (page, output.next_token().map(str::to_owned))
                }
                InventoryKind::Disk => {
                    let output = timeout_at(
                        deadline,
                        Box::pin(
                            self.ec2
                                .describe_volumes()
                                .max_results(500)
                                .set_next_token(next_token)
                                .send(),
                        ),
                    )
                    .await
                    .map_err(|_| failure("timeout", "AWS inventory timed out", vec![]))?
                    .map_err(|error| sdk_failure(&error, "AWS disk inventory failed", vec![]))?;
                    let page = output
                        .volumes()
                        .iter()
                        .map(|volume| {
                            let id = required_id(volume.volume_id(), "vol-")?;
                            let attached_to = volume
                                .attachments()
                                .iter()
                                .map(|attachment| {
                                    required_id(attachment.instance_id(), "i-").map(str::to_owned)
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            Ok(InventoryResource {
                                id: id.to_owned(),
                                incarnation: None,
                                name: id.to_owned(),
                                location: volume
                                    .availability_zone()
                                    .unwrap_or(&self.settings.region)
                                    .to_owned(),
                                ownership: ownership(volume.tags(), controller_id),
                                state: volume.state().map(|state| state.as_str().to_owned()),
                                attached_to,
                                dependencies: Vec::new(),
                            })
                        })
                        .collect::<Result<Vec<_>, CloudError>>()?;
                    (page, output.next_token().map(str::to_owned))
                }
                InventoryKind::AllocatedAddress => {
                    // DescribeAddresses has no pagination parameters in EC2's API.
                    let output =
                        timeout_at(deadline, Box::pin(self.ec2.describe_addresses().send()))
                            .await
                            .map_err(|_| failure("timeout", "AWS inventory timed out", vec![]))?
                            .map_err(|error| {
                                sdk_failure(&error, "AWS address inventory failed", vec![])
                            })?;
                    let page = output
                        .addresses()
                        .iter()
                        .map(|address| {
                            let id = required_id(address.allocation_id(), "eipalloc-")?;
                            let mut attached_to = Vec::new();
                            if let Some(instance) = address.instance_id() {
                                attached_to.push(required_id(Some(instance), "i-")?.to_owned());
                            }

                            if let Some(interface) = address.network_interface_id() {
                                attached_to.push(required_id(Some(interface), "eni-")?.to_owned());
                            }
                            Ok(InventoryResource {
                                id: id.to_owned(),
                                incarnation: None,
                                name: id.to_owned(),
                                location: self.settings.region.clone(),
                                ownership: ownership(address.tags(), controller_id),
                                state: address.association_id().map(|_| "associated".to_owned()),
                                attached_to,
                                dependencies: Vec::new(),
                            })
                        })
                        .collect::<Result<Vec<_>, CloudError>>()?;
                    (page, None)
                }
            };
            if resources.len().saturating_add(page.len()) > 10_000 {
                return Err(failure(
                    "inventory_limit",
                    "AWS inventory exceeds the resource limit",
                    vec![],
                ));
            }

            for resource in page {
                if !ids.insert(resource.id.clone()) {
                    return Err(failure(
                        "pagination",
                        "AWS inventory repeats a resource",
                        vec![],
                    ));
                }

                resources.push(resource);
            }

            let Some(token) = token.filter(|token| !token.is_empty()) else {
                break;
            };
            if !tokens.insert(token.clone()) || tokens.len() > 1000 {
                return Err(failure(
                    "pagination",
                    "AWS inventory pagination did not make progress",
                    vec![],
                ));
            }

            next_token = Some(token);
        }

        Ok(InventoryCollection {
            scope: InventoryScope {
                cloud: "aws".to_owned(),
                account: self.settings.account_id.clone(),
                location: self.settings.region.clone(),
            },
            kind,
            resources,
        })
    }
}

fn required_id<'a>(value: Option<&'a str>, prefix: &str) -> Result<&'a str, CloudError> {
    value
        .filter(|id| {
            id.strip_prefix(prefix).is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        })
        .ok_or_else(|| {
            failure(
                "protocol",
                "AWS inventory lacks a valid native resource ID",
                vec![],
            )
        })
}

fn ownership(tags: &[Tag], controller_id: &str) -> InventoryOwnership {
    InventoryOwnership::from_tag(
        tags.iter()
            .find(|tag| tag.key() == Some("GARM_CONTROLLER_ID"))
            .and_then(|tag| tag.value()),
        controller_id,
    )
}
