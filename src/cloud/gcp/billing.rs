use super::discovery::unix_now;
use super::{
    BTreeSet, BillingGeoType, BillingMoney, BillingPricingInfo, BillingSku, BillingSkuPage,
    CloudError, Decimal, GcpBackend, GcpImage, GcpLicenseBasis, GcpLicensePrice, GcpMachineType,
    GcpPriceSet, Instant, PriceComponent, failure, unique_sku,
};

impl GcpBackend {
    pub(super) async fn price_set(
        &self,
        machine: &GcpMachineType,
        disk_class: &str,
        disk_gib: u32,
        image: &GcpImage,
        purchasing: &str,
        deadline: Instant,
    ) -> Result<GcpPriceSet, CloudError> {
        let service = "services/6F81-5844-456A";
        let base = format!("/v1/{service}/skus?pageSize=5000");
        let mut pages = Vec::new();
        let mut next: Option<String> = None;
        let mut seen_tokens = BTreeSet::new();
        let mut req_ids = Vec::new();
        loop {
            let path = if let Some(token) = next.as_ref() {
                format!("{base}&pageToken={}", urlencoding::encode(token))
            } else {
                base.clone()
            };
            let (page, request_id): (BillingSkuPage, Option<String>) = self
                .request_json(&self.settings.billing_endpoint, &path, deadline)
                .await?;
            pages.extend(page.skus);
            if let Some(id) = request_id {
                req_ids.push(id);
            }
            next = page.next_page_token.filter(|token| !token.is_empty());
            if next.is_none() {
                break;
            }
            if !seen_tokens.insert(next.clone().unwrap_or_default()) || seen_tokens.len() > 1000 {
                return Err(failure(
                    "pagination",
                    "GCP Cloud Billing pagination did not make bounded progress",
                ));
            }
        }
        let zone_region = self
            .settings
            .zone
            .rsplit_once('-')
            .map(|(region, _)| region)
            .unwrap_or(&self.settings.zone);
        let rates = pages
            .into_iter()
            .filter(|sku| {
                sku.geo_taxonomy
                    .as_ref()
                    .is_some_and(|geo| matches!(geo.r#type, BillingGeoType::Global))
                    || sku
                        .service_regions
                        .iter()
                        .any(|region| region.eq_ignore_ascii_case(zone_region))
            })
            .collect::<Vec<_>>();
        let market_matches = |sku: &BillingSku| match purchasing {
            "spot" => sku.category.usage_type.eq_ignore_ascii_case("Preemptible"),
            "on_demand" => sku.category.usage_type.eq_ignore_ascii_case("OnDemand"),
            _ => false,
        };
        let machine_facts = self.settings.machine_facts.get(&machine.name);
        let family = machine
            .name
            .split('-')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mut compute_effective_at = 0u64;
        let mut compute_sources = Vec::new();
        let mut compute = if machine.is_shared_cpu {
            let expected = machine_facts
                .and_then(|facts| facts.shared_cpu_billing_sku.as_deref())
                .ok_or_else(|| {
                    failure(
                        "pricing",
                        "shared-core machine has no exact whole-machine billing SKU attestation",
                    )
                })?;
            let selected = unique_sku(&rates, |sku| sku.name == expected && market_matches(sku))?;
            let (rate, effective) = sku_hourly(selected)?;
            compute_effective_at = effective;
            compute_sources.push(selected.name.clone());
            rate
        } else {
            let cpu = unique_sku(&rates, |sku| {
                sku.category.resource_group.eq_ignore_ascii_case("CPU")
                    && sku.category.resource_family.eq_ignore_ascii_case("Compute")
                    && sku
                        .description
                        .split_whitespace()
                        .next()
                        .is_some_and(|token| token.eq_ignore_ascii_case(&family))
                    && market_matches(sku)
            })?;
            let ram = unique_sku(&rates, |sku| {
                sku.category.resource_group.eq_ignore_ascii_case("RAM")
                    && sku.category.resource_family.eq_ignore_ascii_case("Compute")
                    && sku
                        .description
                        .split_whitespace()
                        .next()
                        .is_some_and(|token| token.eq_ignore_ascii_case(&family))
                    && market_matches(sku)
            })?;
            let (cpu_unit_rate, cpu_effective) = sku_hourly(cpu)?;
            let cpu_rate = cpu_unit_rate
                .checked_mul(Decimal::from(machine.guest_cpus))
                .ok_or_else(|| failure("pricing", "GCP CPU price overflow"))?;
            let memory_gib = Decimal::from(machine.memory_mb) / Decimal::from(1024u32);
            let (ram_unit_rate, ram_effective) = sku_hourly(ram)?;
            compute_effective_at = conservative_effective(compute_effective_at, cpu_effective);
            compute_effective_at = conservative_effective(compute_effective_at, ram_effective);
            compute_sources.push(cpu.name.clone());
            compute_sources.push(ram.name.clone());
            let memory_rate = ram_unit_rate
                .checked_mul(memory_gib)
                .ok_or_else(|| failure("pricing", "GCP RAM price overflow"))?;
            cpu_rate
                .checked_add(memory_rate)
                .ok_or_else(|| failure("pricing", "GCP compute price overflow"))?
        };
        for accelerator in &machine.accelerators {
            let facts = self
                .settings
                .accelerator_facts
                .get(&accelerator.guest_accelerator_type)
                .ok_or_else(|| {
                    failure(
                        "pricing",
                        "GCP accelerator has no exact billing SKU attestation",
                    )
                })?;
            let gpu = unique_sku(&rates, |sku| {
                sku.name == facts.spot_billing_sku
                    && sku.category.resource_family.eq_ignore_ascii_case("Compute")
                    && sku.category.resource_group.eq_ignore_ascii_case("GPU")
                    && market_matches(sku)
            })?;
            let (gpu_unit_rate, gpu_effective) = sku_hourly(gpu)?;
            let gpu_rate = gpu_unit_rate
                .checked_mul(Decimal::from(accelerator.guest_accelerator_count))
                .ok_or_else(|| failure("pricing", "GCP GPU price overflow"))?;
            compute = compute
                .checked_add(gpu_rate)
                .ok_or_else(|| failure("pricing", "GCP compute price overflow"))?;
            compute_effective_at = conservative_effective(compute_effective_at, gpu_effective);
            compute_sources.push(gpu.name.clone());
        }
        let disk_sku_name = self
            .settings
            .storage_billing_skus
            .get(disk_class)
            .ok_or_else(|| failure("pricing", "GCP boot disk has no exact billing SKU binding"))?;
        let disk_sku = unique_sku(&rates, |sku| {
            sku.name == *disk_sku_name && sku.category.usage_type.eq_ignore_ascii_case("OnDemand")
        })?;
        let (disk, disk_effective) =
            sku_rate_for_quantity(disk_sku, Decimal::from(disk_gib), disk_gib)?;
        let mut disk_effective_at = disk_effective;
        let mut disk_sources = vec![disk_sku.name.clone()];
        let local_ssd = if let Some(facts) =
            machine_facts.filter(|facts| facts.bundled_local_ssd_gib.is_some())
        {
            let sku_name = facts.local_ssd_billing_sku.as_deref().ok_or_else(|| {
                failure(
                    "pricing",
                    "bundled Local SSD has no billing SKU attestation",
                )
            })?;
            let sku = unique_sku(&rates, |sku| sku.name == sku_name && market_matches(sku))?;
            let quantity = facts.local_ssd_billing_quantity.ok_or_else(|| {
                failure("pricing", "bundled Local SSD billing quantity is unknown")
            })?;
            let (rate, effective) = sku_rate_for_quantity(
                sku,
                quantity,
                facts.bundled_local_ssd_gib.unwrap_or_default(),
            )?;
            disk_effective_at = conservative_effective(disk_effective_at, effective);
            disk_sources.push(sku.name.clone());
            rate
        } else {
            Decimal::ZERO
        };
        let license = image_license_hourly(&rates, image, machine.guest_cpus, &machine.name)?;
        let (public_ip, public_ip_source, public_ip_effective_at) = if self.child.external_ip_access
        {
            let name = self
                .settings
                .external_ip_billing_sku
                .as_deref()
                .ok_or_else(|| {
                    failure("pricing", "external IPv4 is enabled without a billing SKU")
                })?;
            let sku = unique_sku(&rates, |sku| {
                sku.name == name && sku.category.usage_type.eq_ignore_ascii_case("OnDemand")
            })?;
            let (rate, effective) = sku_hourly(sku)?;
            (rate, Some(sku.name.clone()), Some(effective))
        } else {
            (Decimal::ZERO, None, None)
        };
        Ok(GcpPriceSet {
            compute,
            compute_source: compute_sources.join(","),
            compute_effective_at,
            disk,
            local_ssd,
            disk_source: disk_sources.join(","),
            disk_effective_at,
            license: license.hourly_rate,
            license_source: license.source,
            license_effective_at: license.effective_at,
            public_ip,
            public_ip_source,
            public_ip_effective_at,
            request_ids: req_ids,
        })
    }
}

pub(super) fn effective_price(sku: &BillingSku) -> Result<(&BillingPricingInfo, u64), CloudError> {
    let now = unix_now();
    let mut current = sku
        .pricing_info
        .iter()
        .filter_map(|info| {
            let timestamp = chrono::DateTime::parse_from_rfc3339(&info.effective_time)
                .ok()?
                .timestamp();
            (timestamp >= 0 && timestamp as u64 <= now).then_some((info, timestamp as u64))
        })
        .collect::<Vec<_>>();
    current.sort_by_key(|(_, timestamp)| *timestamp);
    let (_, timestamp) = current.last().copied().ok_or_else(|| {
        failure(
            "pricing",
            "GCP billing SKU has no currently effective price",
        )
    })?;
    let selected = current
        .iter()
        .filter(|(_, effective)| *effective == timestamp)
        .map(|(info, _)| *info)
        .collect::<Vec<_>>();
    let first = selected.first().copied().ok_or_else(|| {
        failure(
            "pricing",
            "GCP billing SKU has no currently effective price",
        )
    })?;
    if selected.iter().any(|info| {
        info.pricing_expression != first.pricing_expression
            || info.aggregation_info != first.aggregation_info
    }) {
        return Err(failure(
            "pricing",
            "GCP billing SKU has conflicting current pricing semantics",
        ));
    }
    let tiers = &first.pricing_expression.tiered_rates;
    if tiers
        .iter()
        .filter(|tier| tier.start_usage_amount == Decimal::ZERO)
        .count()
        != 1
        || tiers
            .iter()
            .any(|tier| tier.start_usage_amount > Decimal::ZERO)
    {
        return Err(failure(
            "pricing",
            "GCP billing SKU has unsupported progressive tiers",
        ));
    }
    let tier = tiers
        .first()
        .ok_or_else(|| failure("pricing", "GCP billing SKU has no zero-usage tier"))?;
    decimal_money(&tier.unit_price)?;
    Ok((first, timestamp))
}

pub(super) fn sku_hourly(sku: &BillingSku) -> Result<(Decimal, u64), CloudError> {
    let (info, effective) = effective_price(sku)?;
    let tier = info
        .pricing_expression
        .tiered_rates
        .iter()
        .find(|tier| tier.start_usage_amount == Decimal::ZERO)
        .ok_or_else(|| failure("pricing", "GCP billing SKU has no zero-usage tier"))?;
    let amount = decimal_money(&tier.unit_price)?;
    let hourly = match info.pricing_expression.usage_unit.as_str() {
        "h" | "GiBy.h" => amount,
        "mo" | "GiBy.mo" => amount
            .checked_div(Decimal::from(730u32))
            .ok_or_else(|| failure("pricing", "GCP monthly price normalization failed"))?,
        _ => {
            return Err(failure(
                "pricing",
                "GCP billing SKU has an unsupported usage unit",
            ));
        }
    };
    Ok((hourly, effective))
}

pub(super) fn sku_rate_for_quantity(
    sku: &BillingSku,
    quantity: Decimal,
    disk_gib: u32,
) -> Result<(Decimal, u64), CloudError> {
    let (info, effective) = effective_price(sku)?;
    let tier = info
        .pricing_expression
        .tiered_rates
        .iter()
        .find(|tier| tier.start_usage_amount == Decimal::ZERO)
        .ok_or_else(|| failure("pricing", "GCP billing SKU has no zero-usage tier"))?;
    let amount = decimal_money(&tier.unit_price)?;
    let hourly = match info.pricing_expression.usage_unit.as_str() {
        "GiBy.h" => amount.checked_mul(Decimal::from(disk_gib)),
        "GiBy.mo" => amount
            .checked_mul(Decimal::from(disk_gib))
            .and_then(|monthly| monthly.checked_div(Decimal::from(730u32))),
        "h" => amount.checked_mul(quantity),
        _ => {
            return Err(failure(
                "pricing",
                "GCP disk billing SKU has an unsupported usage unit",
            ));
        }
    }
    .ok_or_else(|| failure("pricing", "GCP disk price overflow"))?;
    Ok((hourly, effective))
}

pub(super) fn decimal_money(money: &BillingMoney) -> Result<Decimal, CloudError> {
    if money.currency_code != "USD" {
        return Err(failure("pricing", "GCP Cloud Billing SKU is not in USD"));
    }
    let units = Decimal::from_str_exact(&money.units)
        .map_err(|_| failure("pricing", "GCP billing units are invalid"))?;
    let nanos = Decimal::from(money.nanos)
        .checked_div(Decimal::from(1_000_000_000u64))
        .ok_or_else(|| failure("pricing", "GCP billing nanos are invalid"))?;
    units
        .checked_add(nanos)
        .ok_or_else(|| failure("pricing", "GCP price overflow"))
}

pub(super) fn image_license_hourly(
    skus: &[BillingSku],
    image: &GcpImage,
    vcpu: u32,
    machine_type: &str,
) -> Result<GcpLicensePrice, CloudError> {
    let mut total = Decimal::ZERO;
    let mut effective_at: Option<u64> = None;
    let mut sources = Vec::new();
    for (license_id, facts) in &image.license_facts {
        let Some(sku_name) = facts.billing_sku.as_deref() else {
            sources.push(format!("included image license {license_id}"));
            continue;
        };
        let sku = unique_sku(skus, |sku| {
            sku.name == sku_name
                && sku.category.resource_family.eq_ignore_ascii_case("License")
                && sku.category.usage_type.eq_ignore_ascii_case("OnDemand")
        })?;
        let (unit_rate, effective) = sku_hourly(sku)?;
        let quantity = match facts.basis {
            GcpLicenseBasis::Included => Decimal::ZERO,
            GcpLicenseBasis::PerVcpu => Decimal::from(
                if machine_type == "f1-micro" || machine_type == "g1-small" {
                    1
                } else {
                    vcpu
                },
            ),
            GcpLicenseBasis::PerInstance => Decimal::ONE,
            GcpLicenseBasis::SqlVcpuFloor => Decimal::from(vcpu.max(4)),
        };
        if license_id.to_ascii_lowercase().contains("sql")
            && facts.basis != GcpLicenseBasis::SqlVcpuFloor
            && facts.basis != GcpLicenseBasis::Included
        {
            return Err(failure(
                "configuration",
                "SQL image license must use the SQL vCPU floor basis",
            ));
        }
        let charge = unit_rate
            .checked_mul(quantity)
            .ok_or_else(|| failure("pricing", "GCP license price overflow"))?;
        total = total
            .checked_add(charge)
            .ok_or_else(|| failure("pricing", "GCP license price overflow"))?;
        effective_at = Some(effective_at.map_or(effective, |current| current.min(effective)));
        sources.push(format!("image license {license_id} via {sku_name}"));
    }
    Ok(GcpLicensePrice {
        hourly_rate: total,
        source: if sources.is_empty() {
            "GCP image license metadata".to_owned()
        } else {
            sources.join(",")
        },
        effective_at,
    })
}

pub(super) fn conservative_effective(current: u64, next: u64) -> u64 {
    if current == 0 {
        next
    } else {
        current.min(next)
    }
}

pub(super) fn component(
    rate: Decimal,
    source: &str,
    minimum: u64,
    effective: Option<u64>,
) -> PriceComponent {
    PriceComponent {
        hourly_rate: Some(rate),
        minimum_commitment_seconds: minimum,
        source: source.to_owned(),
        observed_at_unix_seconds: Some(unix_now()),
        effective_at_unix_seconds: effective,
        included_in_compute: false,
        exclusions: Vec::new(),
        effective_price_override: None,
    }
}

pub(super) fn included_component(source: &str) -> PriceComponent {
    PriceComponent {
        hourly_rate: Some(Decimal::ZERO),
        minimum_commitment_seconds: 0,
        source: source.to_owned(),
        observed_at_unix_seconds: Some(unix_now()),
        effective_at_unix_seconds: None,
        included_in_compute: true,
        exclusions: Vec::new(),
        effective_price_override: None,
    }
}
