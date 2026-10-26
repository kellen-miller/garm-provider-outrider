use super::{
    Arc, AzureBackend, AzureImage, AzureImageLicensing, AzurePriceRequest, AzurePriceSet,
    AzureRetailSnapshot, AzureShape, BTreeSet, CloudError, Decimal, Duration, Instant,
    MAX_OS_DISK_GIB, PriceComponent, PriceQuote, RETAIL_API, RetailMeter, SystemTime, UNIX_EPOCH,
    failure, timeout_at,
};

impl AzureBackend {
    pub(super) async fn fetch_retail_meters(
        &self,
        location: &str,
        machine_types: &[String],
        deadline: Instant,
    ) -> Result<Vec<RetailMeter>, CloudError> {
        if machine_types.is_empty() {
            return Err(failure(
                "configuration",
                "Azure pricing requires a discovered VM scope",
            ));
        }

        let names = machine_types
            .iter()
            .map(|name| format!("armSkuName eq '{}'", name.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(" or ");
        let compute_filter = format!("(serviceName eq 'Virtual Machines' and ({names}))");
        let filter = format!(
            "({compute_filter} or (serviceName eq 'Storage' and (productName eq 'Standard HDD Managed Disks' or productName eq 'Standard SSD Managed Disks' or productName eq 'Premium SSD Managed Disks')) or (serviceName eq 'Virtual Network' and productName eq 'IP Addresses')) and armRegionName eq '{}' and priceType eq 'Consumption'",
            location.replace('\'', "''")
        );
        let url = format!(
            "{}/api/retail/prices?api-version={}&currencyCode='USD'&$filter={}",
            self.settings.retail_endpoint.trim_end_matches('/'),
            RETAIL_API,
            urlencoding::encode(&filter)
        );
        let mut next = Some(url);
        let mut meters = Vec::new();
        let mut visited = BTreeSet::new();
        while let Some(url) = next {
            if !visited.insert(url.clone()) || visited.len() > 1000 {
                return Err(failure(
                    "pagination",
                    "Azure Retail pagination did not make bounded progress",
                ));
            }
            let page = self.get_retail(&url, deadline).await?;
            meters.extend(page.items);
            next = page.next_page_link;
        }
        Ok(meters)
    }

    pub(super) async fn retail_snapshot(
        &self,
        location: &str,
        machine_types: &[String],
        maximum_price_age: Duration,
        deadline: Instant,
    ) -> Result<AzureRetailSnapshot, CloudError> {
        let mut snapshots = timeout_at(deadline, self.retail_snapshots.lock())
            .await
            .map_err(|_| failure("timeout", "Azure price snapshot lock timed out"))?;
        if maximum_price_age > Duration::ZERO
            && let Some(snapshot) = snapshots.get(location)
            && snapshot.observed_at.elapsed() < maximum_price_age
            && snapshot.fetched_at.elapsed() < Duration::from_secs(60)
            && machine_types
                .iter()
                .all(|name| snapshot.machine_types.contains(name))
        {
            return Ok(snapshot.clone());
        }

        // Serialize refreshes so concurrent analyses share one complete snapshot.
        let observed_at = Instant::now();
        let observed_at_unix_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map_or(0, |value| value.as_secs());
        let meters = self
            .fetch_retail_meters(location, machine_types, deadline)
            .await?;
        if maximum_price_age > Duration::ZERO && observed_at.elapsed() >= maximum_price_age {
            return Err(failure(
                "stale",
                "Azure price fetch exceeded the permitted source age",
            ));
        }

        let snapshot = AzureRetailSnapshot {
            meters: Arc::new(meters),
            observed_at,
            observed_at_unix_seconds,
            fetched_at: Instant::now(),
            machine_types: machine_types.iter().cloned().collect(),
        };
        if maximum_price_age > Duration::ZERO {
            snapshots.insert(location.to_owned(), snapshot.clone());
        }
        Ok(snapshot)
    }
}

pub(super) fn match_prices(
    meters: &[RetailMeter],
    request: AzurePriceRequest<'_>,
) -> Option<AzurePriceSet> {
    let mut compute: Vec<_> = meters
        .iter()
        .filter(|meter| {
            let market_name =
                format!("{} {}", meter.product_name, meter.sku_name).to_ascii_lowercase();
            meter.service_name == "Virtual Machines"
                && meter.product_name.starts_with("Virtual Machines ")
                && meter.arm_sku_name.eq_ignore_ascii_case(request.sku)
                && meter.arm_region_name.eq_ignore_ascii_case(request.location)
                && meter.currency_code == "USD"
                && meter.unit_of_measure == "1 Hour"
                && meter.kind == "Consumption"
                && meter.tier_minimum_units == Decimal::ZERO
                && meter.is_primary_meter_region
                && !market_name.contains("low priority")
                && !market_name.contains("devtest")
                && (!meter.product_name.ends_with(" Windows") || request.os == "windows")
                && (!meter.sku_name.contains("Spot") || request.purchasing == "spot")
                && (meter.sku_name.contains("Spot") == (request.purchasing == "spot"))
                && (request.os != "windows" || meter.product_name.ends_with(" Windows"))
                && (request.os != "linux" || !meter.product_name.ends_with(" Windows"))
        })
        .cloned()
        .collect();
    compute.retain(|meter| {
        effective_now(&meter.effective_start_date)
            && meter.unit_price >= Decimal::ZERO
            && meter.retail_price >= Decimal::ZERO
    });
    let latest = compute
        .iter()
        .map(|meter| meter.effective_start_date.as_str())
        .max()?
        .to_owned();
    compute.retain(|meter| meter.effective_start_date == latest);
    if compute.len() != 1 {
        return None;
    }
    let compute = compute.pop()?;
    let disk_tier = disk_meter_tier(request.disk_sku, request.disk_gib)?;
    let disk_product = match request.disk_sku {
        "Standard_LRS" => "Standard HDD Managed Disks",
        "StandardSSD_LRS" => "Standard SSD Managed Disks",
        "Premium_LRS" => "Premium SSD Managed Disks",
        _ => return None,
    };
    let disk = unique_meter(meters, |meter| {
        meter.service_name == "Storage"
            && meter.product_name == disk_product
            && meter
                .sku_name
                .eq_ignore_ascii_case(&format!("{disk_tier} LRS"))
            && meter
                .meter_name
                .eq_ignore_ascii_case(&format!("{disk_tier} LRS Disk"))
            && meter.is_primary_meter_region
            && meter.arm_region_name.eq_ignore_ascii_case(request.location)
            && meter.unit_of_measure == "1/Month"
            && meter.currency_code == "USD"
            && meter.kind == "Consumption"
            && meter.tier_minimum_units == Decimal::ZERO
    })?;
    let public_ip = unique_meter(meters, |meter| {
        meter.service_name == "Virtual Network"
            && meter.product_name == "IP Addresses"
            && meter.sku_name == "Standard"
            && meter.meter_name == "Standard IPv4 Static Public IP"
            && meter.arm_region_name.eq_ignore_ascii_case(request.location)
            && meter.currency_code == "USD"
            && meter.unit_of_measure == "1 Hour"
            && meter.kind == "Consumption"
            && meter.tier_minimum_units == Decimal::ZERO
    });
    if request.os == "windows"
        && request.licensing != AzureImageLicensing::WindowsLicenseInComputeMeter
    {
        return None;
    }
    if request.os == "linux" && request.licensing != AzureImageLicensing::NoAdditionalSoftwareCharge
    {
        return None;
    }
    if request.licensing_source.trim().is_empty() {
        return None;
    }
    if request.public_requested && public_ip.is_none() {
        return None;
    }
    Some(AzurePriceSet {
        compute,
        disk,
        public_ip,
        licensing_included: request.os == "windows",
        licensing_source: request.licensing_source.to_owned(),
        licensing_description: match request.licensing {
            AzureImageLicensing::NoAdditionalSoftwareCharge => {
                "No additional image software charge".to_owned()
            }
            AzureImageLicensing::WindowsLicenseInComputeMeter => {
                "Windows license included in the compute meter".to_owned()
            }
            AzureImageLicensing::AdditionalUnpricedSoftware => return None,
        },
    })
}

pub fn disk_meter_tier(storage_sku: &str, size_gib: u32) -> Option<&'static str> {
    let (tiers, limits): (&[&str], &[u32]) = match storage_sku {
        "Standard_LRS" => (
            &[
                "S4", "S6", "S10", "S15", "S20", "S30", "S40", "S50", "S60", "S70", "S80",
            ],
            &[32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32767],
        ),
        "StandardSSD_LRS" => (
            &[
                "E1", "E2", "E3", "E4", "E6", "E10", "E15", "E20", "E30", "E40", "E50", "E60",
                "E70", "E80",
            ],
            &[
                4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32767,
            ],
        ),
        "Premium_LRS" => (
            &[
                "P1", "P2", "P3", "P4", "P6", "P10", "P15", "P20", "P30", "P40", "P50", "P60",
                "P70", "P80",
            ],
            &[
                4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32767,
            ],
        ),
        _ => return None,
    };
    let tier = limits.iter().position(|limit| size_gib <= *limit)?;
    tiers.get(tier).copied()
}

pub(super) fn disk_performance(storage_sku: &str, size_gib: u32) -> Option<(u32, Decimal)> {
    let tier = disk_meter_tier(storage_sku, size_gib)?;
    let (iops, throughput) = match (storage_sku, tier) {
        ("Standard_LRS", "S4" | "S6" | "S10" | "S15" | "S20" | "S30" | "S40" | "S50") => (500, 60),
        ("Standard_LRS", "S60") => (1_300, 300),
        ("Standard_LRS", "S70" | "S80") => (2_000, 500),
        (
            "StandardSSD_LRS",
            "E1" | "E2" | "E3" | "E4" | "E6" | "E10" | "E15" | "E20" | "E30" | "E40" | "E50",
        ) => (500, 100),
        ("StandardSSD_LRS", "E60") => (2_000, 400),
        ("StandardSSD_LRS", "E70") => (4_000, 600),
        ("StandardSSD_LRS", "E80") => (6_000, 750),
        ("Premium_LRS", "P1" | "P2" | "P3" | "P4") => (120, 25),
        ("Premium_LRS", "P6") => (240, 50),
        ("Premium_LRS", "P10") => (500, 100),
        ("Premium_LRS", "P15") => (1100, 125),
        ("Premium_LRS", "P20") => (2300, 150),
        ("Premium_LRS", "P30") => (5000, 200),
        ("Premium_LRS", "P40" | "P50") => (7500, 250),
        ("Premium_LRS", "P60") => (16000, 500),
        ("Premium_LRS", "P70") => (18000, 750),
        ("Premium_LRS", "P80") => (20000, 900),
        _ => return None,
    };
    Some((
        iops,
        Decimal::from(throughput) * Decimal::from(1_000_000) / Decimal::from(1_048_576),
    ))
}

pub(super) fn requested_disk_performance(
    profile: &crate::config::RunnerProfile,
    shape: &AzureShape,
    image: &AzureImage,
    disk_sku: &str,
) -> Option<(u32, u32, Decimal)> {
    if disk_sku == "Premium_LRS" && shape.premium_io_supported != Some(true) {
        return None;
    }
    let size = profile.boot_disk.size_gib.max(image.minimum_os_disk_gib);
    if disk_meter_tier(disk_sku, size).is_none()
        || size > MAX_OS_DISK_GIB
        || size > profile.administrator_limits.max_boot_disk_gib
    {
        return None;
    }
    let (tier_iops, tier_throughput) = disk_performance(disk_sku, size)?;
    let iops = tier_iops.min(shape.max_disk_iops.unwrap_or_default());
    let throughput = tier_throughput.min(shape.max_disk_throughput_mib_s.unwrap_or_default());
    if (profile.boot_disk.iops.unwrap_or_default() > 0 && shape.max_disk_iops.is_none())
        || (profile.boot_disk.throughput_mib_s.unwrap_or_default() > 0
            && shape.max_disk_throughput_mib_s.is_none())
        || iops < profile.boot_disk.iops.unwrap_or_default()
        || throughput < Decimal::from(profile.boot_disk.throughput_mib_s.unwrap_or_default())
    {
        return None;
    }
    Some((size, iops, throughput))
}

pub(super) fn unique_meter(
    meters: &[RetailMeter],
    predicate: impl Fn(&RetailMeter) -> bool,
) -> Option<RetailMeter> {
    let mut matches: Vec<_> = meters
        .iter()
        .filter(|meter| {
            predicate(meter)
                && effective_now(&meter.effective_start_date)
                && meter.unit_price >= Decimal::ZERO
                && meter.retail_price >= Decimal::ZERO
        })
        .cloned()
        .collect();
    let latest = matches
        .iter()
        .map(|meter| meter.effective_start_date.as_str())
        .max()?
        .to_owned();
    matches.retain(|meter| meter.effective_start_date == latest);
    if matches.len() != 1 {
        return None;
    }
    let rate = matches.first()?.unit_price;
    if matches.iter().any(|meter| meter.unit_price != rate) {
        return None;
    }
    matches.into_iter().next()
}

pub(super) fn effective_now(value: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(value)
        .is_ok_and(|date| date.timestamp() <= chrono::Utc::now().timestamp())
}

pub(super) fn quote_prices(prices: &AzurePriceSet, public: bool, observed: u64) -> PriceQuote {
    let compute = prices.compute.unit_price;
    // Standard disks also charge for I/O transactions. Their complete cost
    // remains unknown until a workload transaction estimate is available.
    let disk = (prices.disk.product_name == "Premium SSD Managed Disks")
        .then(|| prices.disk.unit_price / Decimal::from(730));
    let public_rate = if public {
        prices.public_ip.as_ref().map(|meter| meter.unit_price)
    } else {
        Some(Decimal::ZERO)
    };
    let mut quote = PriceQuote {
        provider_request_ids: Vec::new(),
        currency: "USD".to_owned(),
        compute: PriceComponent {
            hourly_rate: Some(compute),
            minimum_commitment_seconds: 60,
            source: format!("Azure Retail Prices API meter {}", prices.compute.meter_id),
            observed_at_unix_seconds: Some(observed),
            effective_at_unix_seconds: date_epoch(&prices.compute.effective_start_date),
            included_in_compute: false,
            exclusions: vec![],
            effective_price_override: None,
        },
        disk: PriceComponent {
            hourly_rate: disk,
            minimum_commitment_seconds: 3600,
            source: if disk.is_some() {
                format!("Azure Retail Prices API meter {}", prices.disk.meter_id)
            } else {
                format!(
                    "Azure Retail Prices API meter {}; I/O transaction usage is unpriced",
                    prices.disk.meter_id
                )
            },
            observed_at_unix_seconds: Some(observed),
            effective_at_unix_seconds: date_epoch(&prices.disk.effective_start_date),
            included_in_compute: false,
            exclusions: vec![],
            effective_price_override: None,
        },
        licensing: PriceComponent {
            hourly_rate: Some(Decimal::ZERO),
            minimum_commitment_seconds: 60,
            source: format!(
                "{}; attested by {}; Azure compute meter {}",
                prices.licensing_description, prices.licensing_source, prices.compute.meter_id
            ),
            observed_at_unix_seconds: Some(observed),
            effective_at_unix_seconds: date_epoch(&prices.compute.effective_start_date),
            included_in_compute: prices.licensing_included,
            exclusions: vec![],
            effective_price_override: None,
        },
        public_address: PriceComponent {
            hourly_rate: public_rate,
            minimum_commitment_seconds: 60,
            source: prices.public_ip.as_ref().map_or_else(
                || "Azure Retail Prices API".to_owned(),
                |meter| format!("Azure Retail Prices API meter {}", meter.meter_id),
            ),
            observed_at_unix_seconds: Some(observed),
            effective_at_unix_seconds: prices
                .public_ip
                .as_ref()
                .and_then(|meter| date_epoch(&meter.effective_start_date)),
            included_in_compute: false,
            exclusions: vec![],
            effective_price_override: None,
        },
        new_host_minimum_commitment_seconds: None,
        existing_host_remaining_commitment_seconds: None,
        observed_age_seconds: Some(0),
        included_charge_names: BTreeSet::new(),
        exclusions: vec![],
    };
    quote.refresh_observed_age();
    quote
}

pub(super) fn date_epoch(value: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .and_then(|date| u64::try_from(date.timestamp()).ok())
}
