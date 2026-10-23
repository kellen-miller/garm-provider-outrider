use super::{
    AzureAcceleratorFacts, AzureBackend, AzureImage, AzureImageLicensing, AzureImageVersion,
    AzureShape, AzureSku, AzureSkuPage, BTreeSet, COMPUTE_API, CatalogQuery, CloudError, Decimal,
    Instant, RESOURCE_SKUS_API, failure,
};

impl AzureBackend {
    pub(super) async fn sku_inventory(
        &self,
        location: &str,
        deadline: Instant,
    ) -> Result<Vec<AzureSku>, CloudError> {
        let url = format!(
            "{}/subscriptions/{}/providers/Microsoft.Compute/skus?api-version={}&$filter=location%20eq%20'{}'",
            self.settings.management_endpoint.trim_end_matches('/'),
            self.settings.subscription_id,
            RESOURCE_SKUS_API,
            location
        );
        let prefix = format!(
            "/subscriptions/{}/providers/Microsoft.Compute/skus",
            self.settings.subscription_id
        );
        let mut next = Some(url);
        let mut results = Vec::new();
        let mut visited = BTreeSet::new();
        while let Some(url) = next {
            if !visited.insert(url.clone()) || visited.len() > 1000 {
                return Err(failure(
                    "pagination",
                    "Azure SKU pagination did not make bounded progress",
                ));
            }
            let page: AzureSkuPage =
                serde_json::from_value(self.get_management(&url, &prefix, deadline).await?)
                    .map_err(|_| failure("protocol", "Azure SKU response was invalid"))?;
            results.extend(
                page.value
                    .into_iter()
                    .filter(|sku| sku.resource_type.eq_ignore_ascii_case("virtualMachines")),
            );
            next = page.next_link;
        }
        Ok(results)
    }

    pub(super) async fn image_facts(
        &self,
        image: &AzureImage,
        location: &str,
        deadline: Instant,
    ) -> Result<(String, String, String), CloudError> {
        let parts: Vec<_> = image.image_id.split(':').collect();
        if parts.len() != 4
            || parts.iter().any(|part| part.is_empty())
            || parts[3].eq_ignore_ascii_case("latest")
        {
            return Err(failure(
                "unsupported",
                "Azure image must use an immutable publisher:offer:sku:version reference",
            ));
        }
        let url = format!(
            "{}/subscriptions/{}/providers/Microsoft.Compute/locations/{}/publishers/{}/artifacttypes/vmimage/offers/{}/skus/{}/versions/{}?api-version={}",
            self.settings.management_endpoint.trim_end_matches('/'),
            self.settings.subscription_id,
            location,
            parts[0],
            parts[1],
            parts[2],
            parts[3],
            COMPUTE_API
        );
        let prefix = format!(
            "/subscriptions/{}/providers/Microsoft.Compute/locations/{}/publishers/{}/artifacttypes/vmimage/offers/{}/skus/{}/versions/{}",
            self.settings.subscription_id, location, parts[0], parts[1], parts[2], parts[3]
        );
        let value = self.get_management(&url, &prefix, deadline).await?;
        let version: AzureImageVersion = serde_json::from_value(value)
            .map_err(|_| failure("protocol", "Azure image version response was invalid"))?;
        if version.properties.plan.is_some() {
            return Err(failure(
                "unsupported",
                "Azure marketplace image plans are not dispatched by the pinned provider",
            ));
        }
        let os = match version
            .properties
            .os_disk_image
            .operating_system
            .to_ascii_lowercase()
            .as_str()
        {
            "linux" => "linux",
            "windows" => "windows",
            _ => return Err(failure("unsupported", "Azure image OS is unsupported")),
        };
        let architecture = match version
            .properties
            .architecture
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("x64") | Some("amd64") => "amd64",
            Some("arm64") => "arm64",
            _ => {
                return Err(failure(
                    "unsupported",
                    "Azure image architecture is not reported by immutable image metadata",
                ));
            }
        };
        match (os, image.licensing) {
            ("linux", AzureImageLicensing::NoAdditionalSoftwareCharge)
            | ("windows", AzureImageLicensing::WindowsLicenseInComputeMeter) => {}
            (_, AzureImageLicensing::AdditionalUnpricedSoftware) => {
                return Err(failure(
                    "unsupported",
                    "Azure image software licensing has an additional unpriced charge",
                ));
            }
            _ => {
                return Err(failure(
                    "configuration",
                    "Azure image licensing attestation does not match the image operating system",
                ));
            }
        }
        if image.licensing_source.trim().is_empty() {
            return Err(failure(
                "configuration",
                "Azure image licensing attestation needs source provenance",
            ));
        }
        if image.minimum_os_disk_gib == 0 || image.minimum_os_disk_source.trim().is_empty() {
            return Err(failure(
                "configuration",
                "Azure image minimum OS disk size needs operator provenance",
            ));
        }
        Ok((parts[3].to_owned(), os.to_owned(), architecture.to_owned()))
    }

    pub(super) fn configured_image(&self, alias: &str) -> Result<&AzureImage, CloudError> {
        self.settings
            .images
            .get(alias)
            .ok_or_else(|| failure("configuration", "Azure image alias is not configured"))
    }

    pub(super) fn query_matches_scope(&self, query: &CatalogQuery) -> bool {
        query.clouds.iter().any(|cloud| cloud == "azure")
            && (query.accounts.is_empty() || query.accounts.contains(&self.settings.account_id))
            && (query.profile.accounts.is_empty()
                || query.profile.accounts.contains(&self.settings.account_id))
    }
}

pub(super) fn sku_shape(sku: AzureSku) -> Option<AzureShape> {
    let vcpu = capability_u32(&sku, "vCPUsAvailable").or_else(|| capability_u32(&sku, "vCPUs"))?;
    let cpu_architecture =
        capability_text(&sku, "CpuArchitectureType").and_then(|value| {
            match value.to_ascii_lowercase().as_str() {
                "x64" => Some("amd64".to_owned()),
                "arm64" => Some("arm64".to_owned()),
                _ => None,
            }
        });
    let memory_gib = capability_decimal(&sku, "MemoryGB")?;
    let gpu_count = capability_u32(&sku, "GPUsAvailable")
        .or_else(|| capability_u32(&sku, "GPUs"))
        .unwrap_or_default();
    let reported_gpu_memory_gib = capability_decimal(&sku, "GpuMemoryGB");
    let local_scratch_gib = capability_decimal(&sku, "MaxResourceVolumeMB")
        .map(|mb| mb / Decimal::from(1024))
        .unwrap_or_default();
    let max_disk_iops = capability_u32(&sku, "UncachedDiskIOPS");
    let max_disk_throughput_mib_s = capability_decimal(&sku, "UncachedDiskBytesPerSecond")
        .map(|bytes| bytes / Decimal::from(1_048_576));
    Some(AzureShape {
        ephemeral_disk: sku.capabilities.iter().any(|cap| {
            cap.name == "EphemeralOSDiskSupported" && cap.value.eq_ignore_ascii_case("true")
        }),
        premium_io_supported: capability_text(&sku, "PremiumIO")
            .and_then(|value| value.parse::<bool>().ok()),
        sku,
        vcpu,
        cpu_architecture,
        memory_gib,
        gpu_count,
        reported_gpu_memory_gib,
        local_scratch_gib,
        max_disk_iops,
        max_disk_throughput_mib_s,
    })
}

pub(super) fn capability_u32(sku: &AzureSku, name: &str) -> Option<u32> {
    sku.capabilities
        .iter()
        .find(|cap| cap.name.eq_ignore_ascii_case(name))?
        .value
        .parse()
        .ok()
}

pub(super) fn capability_decimal(sku: &AzureSku, name: &str) -> Option<Decimal> {
    sku.capabilities
        .iter()
        .find(|cap| cap.name.eq_ignore_ascii_case(name))?
        .value
        .parse()
        .ok()
}

pub(super) fn capability_text(sku: &AzureSku, name: &str) -> Option<String> {
    sku.capabilities
        .iter()
        .find(|cap| cap.name.eq_ignore_ascii_case(name))
        .map(|cap| cap.value.clone())
}

pub(super) fn shape_matches_profile(
    shape: &AzureShape,
    profile: &crate::config::RunnerProfile,
    accelerator: Option<&AzureAcceleratorFacts>,
) -> bool {
    shape.vcpu >= profile.vcpu_min
        && shape.vcpu <= profile.vcpu_max
        && shape.memory_gib >= Decimal::from(profile.memory_gib_min)
        && shape.memory_gib <= Decimal::from(profile.memory_gib_max)
        && shape.gpu_count >= profile.gpu.as_ref().map_or(0, |gpu| gpu.count)
        && shape.local_scratch_gib >= Decimal::from(profile.local_scratch_gib)
        && shape.local_scratch_gib
            <= Decimal::from(profile.administrator_limits.max_local_scratch_gib)
        && (shape.gpu_count == 0 || accelerator.is_some())
        && accelerator.is_none_or(|facts| {
            shape.reported_gpu_memory_gib.is_none_or(|reported| {
                reported > Decimal::ZERO && reported == facts.total_memory_gib
            })
        })
        && profile.gpu.as_ref().is_none_or(|gpu| {
            gpu.count == 0
                || accelerator.is_some_and(|facts| {
                    shape.gpu_count >= gpu.count
                        && facts.vendor.eq_ignore_ascii_case(&gpu.vendor)
                        && gpu.model.as_ref().is_none_or(|model| facts.model == *model)
                        && gpu
                            .memory_gib
                            .is_none_or(|memory| facts.total_memory_gib >= Decimal::from(memory))
                })
        })
        && accelerator.is_none_or(|facts| {
            shape.gpu_count <= profile.administrator_limits.max_gpu_count
                && facts.total_memory_gib
                    <= Decimal::from(profile.administrator_limits.max_gpu_memory_gib)
                && profile
                    .administrator_limits
                    .allowed_gpu_vendors
                    .contains(&facts.vendor.to_ascii_lowercase())
                && profile
                    .administrator_limits
                    .allowed_gpu_models
                    .contains(&facts.model)
        })
}
