use super::{
    AwsBackend, AwsComputeQuote, AwsDiskQuote, AwsImage, AwsLocation, AwsPriceProducts,
    AwsQuotedPrices, BTreeSet, Candidate, CloudError, DateTime, DateTimeFormat, Decimal,
    InstanceType, Instant, PriceComponent, PriceQuote, RequestId, failure, sdk_failure, timeout_at,
};

impl AwsBackend {
    pub(super) async fn price_products(
        &self,
        service_code: &str,
        filters: Vec<aws_sdk_pricing::types::Filter>,
        deadline: Instant,
    ) -> Result<AwsPriceProducts, CloudError> {
        let mut token = None;
        let mut products = Vec::new();
        let mut request_ids = Vec::new();
        loop {
            let response = timeout_at(
                deadline,
                self.pricing
                    .get_products()
                    .service_code(service_code)
                    .set_filters(Some(filters.clone()))
                    .set_next_token(token.clone())
                    .max_results(100)
                    .send(),
            )
            .await
            .map_err(|_| failure("timeout", "AWS price request timed out", vec![]))?
            .map_err(|error| sdk_failure(&error, "AWS price request failed", vec![]))?;
            products.extend(response.price_list().iter().cloned());
            if let Some(request_id) = response.request_id() {
                request_ids.push(request_id.to_owned());
            }

            token = response.next_token().map(str::to_owned);
            if token.is_none() {
                break;
            }
        }

        Ok(AwsPriceProducts {
            products,
            request_ids,
        })
    }

    pub(super) async fn fetch_compute_quote(
        &self,
        candidate: &Candidate,
        machine_type: &str,
        location: &AwsLocation,
        usage_operation: &str,
        deadline: Instant,
    ) -> Result<AwsComputeQuote, CloudError> {
        if candidate.purchasing == "spot" {
            let history = timeout_at(
                deadline,
                self.ec2
                    .describe_spot_price_history()
                    .instance_types(InstanceType::from(machine_type))
                    .product_descriptions(if candidate.os == "windows" {
                        "Windows"
                    } else {
                        "Linux/UNIX"
                    })
                    .availability_zone(location.availability_zone.clone())
                    .max_results(10)
                    .send(),
            )
            .await
            .map_err(|_| failure("timeout", "AWS Spot price request timed out", vec![]))?
            .map_err(|error| sdk_failure(&error, "AWS Spot price request failed", vec![]))?;
            let latest = history
                .spot_price_history()
                .iter()
                .filter_map(|entry| {
                    Some((
                        entry.timestamp()?.secs(),
                        entry.spot_price()?.parse::<Decimal>().ok()?,
                    ))
                })
                .max_by_key(|(timestamp, _)| *timestamp);
            return Ok(AwsComputeQuote {
                rate: latest.map(|(_, price)| price),
                effective_at: latest.and_then(|(timestamp, _)| u64::try_from(timestamp).ok()),
                request_ids: history
                    .request_id()
                    .map(str::to_owned)
                    .into_iter()
                    .collect(),
            });
        }

        let filters = vec![
            pricing_filter("instanceType", machine_type)?,
            pricing_filter("location", &self.settings.pricing_location)?,
            pricing_filter("regionCode", &self.settings.region)?,
            pricing_filter(
                "operatingSystem",
                if candidate.os == "windows" {
                    "Windows"
                } else {
                    "Linux"
                },
            )?,
            pricing_filter("preInstalledSw", "NA")?,
            pricing_filter("tenancy", "Shared")?,
            pricing_filter("capacitystatus", "Used")?,
        ];
        let products = self.price_products("AmazonEC2", filters, deadline).await?;
        let matches = |raw: &str| {
            compute_price(
                raw,
                machine_type,
                &candidate.os,
                &self.settings.pricing_location,
                &self.settings.region,
                usage_operation,
            )
        };

        Ok(AwsComputeQuote {
            effective_at: matching_effective_at(&products.products, |raw| matches(raw).is_some()),
            rate: unique_rate(
                products
                    .products
                    .iter()
                    .filter_map(|raw| matches(raw))
                    .collect(),
            ),
            request_ids: products.request_ids,
        })
    }

    pub(super) async fn fetch_disk_quote(
        &self,
        candidate: &Candidate,
        provisioned_throughput: u64,
        deadline: Instant,
    ) -> Result<AwsDiskQuote, CloudError> {
        let products = self
            .price_products(
                "AmazonEC2",
                vec![
                    pricing_filter("location", &self.settings.pricing_location)?,
                    pricing_filter("regionCode", &self.settings.region)?,
                    pricing_filter("volumeApiName", "gp3")?,
                ],
                deadline,
            )
            .await?;
        let storage_rate = unique_rate(
            products
                .products
                .iter()
                .filter_map(|raw| {
                    storage_price(raw, &self.settings.pricing_location, &self.settings.region)
                })
                .collect(),
        );
        let storage_effective_at = matching_effective_at(&products.products, |raw| {
            storage_price(raw, &self.settings.pricing_location, &self.settings.region).is_some()
        });
        let iops = gp3_price(
            &products.products,
            Decimal::from(candidate.boot_disk_iops.saturating_sub(3000)),
            &self.settings.pricing_location,
            &self.settings.region,
            "EBS:VolumeP-IOPS.gp3",
        );
        let throughput = gp3_price(
            &products.products,
            Decimal::from(provisioned_throughput.saturating_sub(125)),
            &self.settings.pricing_location,
            &self.settings.region,
            "EBS:VolumeP-Throughput.gp3",
        );
        let hourly_rate = match (storage_rate, iops.0, throughput.0) {
            (Some(storage), Some(iops), Some(throughput)) => storage
                .checked_mul(Decimal::from(candidate.boot_disk_gib))
                .and_then(|value| value.checked_add(iops))
                .and_then(|value| value.checked_add(throughput))
                .and_then(|value| value.checked_div(Decimal::from(730u32))),
            _ => None,
        };

        Ok(AwsDiskQuote {
            hourly_rate,
            effective_at: [storage_effective_at, iops.1, throughput.1]
                .into_iter()
                .flatten()
                .min(),
            request_ids: products.request_ids,
        })
    }

    pub(super) async fn fetch_public_address_quote(
        &self,
        associate_public_ip: bool,
        deadline: Instant,
    ) -> Result<(Option<Decimal>, Option<u64>, Vec<String>), CloudError> {
        if !associate_public_ip {
            return Ok((Some(Decimal::ZERO), None, Vec::new()));
        }

        let products = self
            .price_products(
                "AmazonVPC",
                vec![
                    pricing_filter("location", &self.settings.pricing_location)?,
                    pricing_filter("regionCode", &self.settings.region)?,
                    pricing_filter("group", "VPCPublicIPv4Address")?,
                ],
                deadline,
            )
            .await?;
        let rate = unique_rate(
            products
                .products
                .iter()
                .filter_map(|raw| {
                    public_ipv4_price(raw, &self.settings.pricing_location, &self.settings.region)
                })
                .collect(),
        );
        let effective_at = matching_effective_at(&products.products, |raw| {
            public_ipv4_price(raw, &self.settings.pricing_location, &self.settings.region).is_some()
        });
        Ok((rate, effective_at, products.request_ids))
    }
}

pub(super) fn assemble_price_quote(
    candidate: &Candidate,
    image: &AwsImage,
    prices: AwsQuotedPrices,
    observed_at: Option<u64>,
) -> PriceQuote {
    let source =
        "AWS public price list and EC2 Spot history; 730-hour monthly estimate basis".to_owned();
    let mut exclusions =
        vec!["taxes, negotiated discounts, and data transfer are excluded".to_owned()];
    if image.windows_license_included && candidate.os == "windows" {
        exclusions.push(
            "Windows license is included only by configured exact AMI/SKU declaration".to_owned(),
        );
    }

    let mut quote = PriceQuote {
        provider_request_ids: prices.provider_request_ids,
        currency: "USD".to_owned(),
        compute: PriceComponent {
            hourly_rate: prices.compute.rate,
            minimum_commitment_seconds: 60,
            source: source.clone(),
            observed_at_unix_seconds: observed_at,
            effective_at_unix_seconds: prices.compute.effective_at,
            included_in_compute: false,
            exclusions: vec![],
            effective_price_override: None,
        },
        disk: PriceComponent {
            hourly_rate: prices.disk.hourly_rate,
            minimum_commitment_seconds: 60,
            source: source.clone(),
            observed_at_unix_seconds: observed_at,
            effective_at_unix_seconds: prices.disk.effective_at,
            included_in_compute: false,
            exclusions: vec![],
            effective_price_override: None,
        },
        licensing: PriceComponent {
            hourly_rate: if candidate.os == "windows" && !image.windows_license_included {
                None
            } else {
                Some(Decimal::ZERO)
            },
            minimum_commitment_seconds: 0,
            source: source.clone(),
            observed_at_unix_seconds: observed_at,
            effective_at_unix_seconds: prices.compute.effective_at,
            included_in_compute: candidate.os == "windows" && image.windows_license_included,
            exclusions: vec![],
            effective_price_override: None,
        },
        public_address: PriceComponent {
            hourly_rate: prices.public_rate,
            minimum_commitment_seconds: 60,
            source,
            observed_at_unix_seconds: observed_at,
            effective_at_unix_seconds: prices.public_effective_at,
            included_in_compute: false,
            exclusions: vec![],
            effective_price_override: None,
        },
        new_host_minimum_commitment_seconds: None,
        existing_host_remaining_commitment_seconds: None,
        observed_age_seconds: Some(0),
        included_charge_names: BTreeSet::new(),
        exclusions,
    };
    quote.refresh_observed_age();
    quote
}

pub(super) fn pricing_filter(
    field: &str,
    value: &str,
) -> Result<aws_sdk_pricing::types::Filter, CloudError> {
    aws_sdk_pricing::types::Filter::builder()
        .field(field)
        .value(value)
        .r#type(aws_sdk_pricing::types::FilterType::TermMatch)
        .build()
        .map_err(|_| failure("configuration", "invalid AWS pricing filter", vec![]))
}

pub(super) fn compute_price(
    raw: &str,
    machine_type: &str,
    os: &str,
    location: &str,
    region: &str,
    usage_operation: &str,
) -> Option<(String, Decimal)> {
    let document: serde_json::Value = serde_json::from_str(raw).ok()?;
    let attributes = document.pointer("/product/attributes")?;
    if attributes.get("instanceType")?.as_str()? != machine_type
        || attributes.get("location")?.as_str()? != location
        || attributes.get("regionCode")?.as_str()? != region
        || attributes.get("operatingSystem")?.as_str()?
            != if os == "windows" { "Windows" } else { "Linux" }
        || attributes.get("preInstalledSw")?.as_str()? != "NA"
        || attributes.get("tenancy")?.as_str()? != "Shared"
        || attributes.get("capacitystatus")?.as_str()? != "Used"
        || attributes.get("operation")?.as_str()? != usage_operation
        || attributes.get("servicecode")?.as_str()? != "AmazonEC2"
        || document.pointer("/product/productFamily")?.as_str()? != "Compute Instance"
        || !attributes
            .get("usagetype")?
            .as_str()?
            .ends_with(&format!("BoxUsage:{machine_type}"))
    {
        return None;
    }

    let sku = document.pointer("/product/sku")?.as_str()?.to_owned();
    Some((sku, active_term_rate(&document, &["Hrs"], false)?.0))
}

pub(super) fn storage_price(raw: &str, location: &str, region: &str) -> Option<(String, Decimal)> {
    let document: serde_json::Value = serde_json::from_str(raw).ok()?;
    let attributes = document.pointer("/product/attributes")?;
    if attributes.get("location")?.as_str()? != location
        || attributes.get("regionCode")?.as_str()? != region
        || attributes.get("volumeApiName")?.as_str()? != "gp3"
        || !attributes
            .get("usagetype")?
            .as_str()?
            .ends_with("EBS:VolumeUsage.gp3")
    {
        return None;
    }

    let sku = document.pointer("/product/sku")?.as_str()?.to_owned();
    Some((sku, active_term_rate(&document, &["GB-Mo"], false)?.0))
}

pub(super) fn gp3_price(
    products: &[String],
    quantity: Decimal,
    location: &str,
    region: &str,
    usage_type: &str,
) -> (Option<Decimal>, Option<u64>) {
    if quantity <= Decimal::ZERO {
        return (Some(Decimal::ZERO), None);
    }

    let rate = unique_rate(
        products
            .iter()
            .filter_map(|raw| gp3_performance_price(raw, location, region, usage_type))
            .collect(),
    );
    let effective_at = matching_effective_at(products, |raw| {
        gp3_performance_price(raw, location, region, usage_type).is_some()
    });
    (
        rate.and_then(|hourly| hourly.checked_mul(quantity)),
        effective_at,
    )
}

pub(super) fn gp3_performance_price(
    raw: &str,
    location: &str,
    region: &str,
    usage_type: &str,
) -> Option<(String, Decimal)> {
    let document: serde_json::Value = serde_json::from_str(raw).ok()?;
    let attributes = document.pointer("/product/attributes")?;
    if attributes.get("location")?.as_str()? != location
        || attributes.get("regionCode")?.as_str()? != region
        || attributes.get("volumeApiName")?.as_str()? != "gp3"
        || !attributes.get("usagetype")?.as_str()?.ends_with(usage_type)
    {
        return None;
    }

    let sku = document.pointer("/product/sku")?.as_str()?.to_owned();
    Some((
        sku,
        active_term_rate(&document, &["IOPS-Mo", "MBps-Mo", "MiBps-Mo"], true)?.0,
    ))
}

pub(super) fn public_ipv4_price(
    raw: &str,
    location: &str,
    region: &str,
) -> Option<(String, Decimal)> {
    let document: serde_json::Value = serde_json::from_str(raw).ok()?;
    let attributes = document.pointer("/product/attributes")?;
    if attributes.get("location")?.as_str()? != location
        || attributes.get("regionCode")?.as_str()? != region
        || attributes.get("group")?.as_str()? != "VPCPublicIPv4Address"
        || !attributes
            .get("usagetype")?
            .as_str()?
            .ends_with("-PublicIPv4:InUseAddress")
    {
        return None;
    }

    let sku = document.pointer("/product/sku")?.as_str()?.to_owned();
    Some((sku, active_term_rate(&document, &["Hrs"], false)?.0))
}

pub(super) fn active_term_rate(
    document: &serde_json::Value,
    units: &[&str],
    positive_only: bool,
) -> Option<(Decimal, u64)> {
    let terms = document.pointer("/terms/OnDemand")?.as_object()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let eligible = terms
        .values()
        .filter_map(|term| Some((effective_date(term)?, term)))
        .filter(|(effective, _)| *effective <= now)
        .collect::<Vec<_>>();
    let effective = eligible.iter().map(|(effective, _)| *effective).max()?;
    let mut active = eligible
        .into_iter()
        .filter(|(date, _)| *date == effective)
        .collect::<Vec<_>>();
    if active.len() != 1 {
        return None;
    }

    let dimensions = active.pop()?.1.get("priceDimensions")?.as_object()?;
    if dimensions.len() != 1 {
        return None;
    }

    let dimension = dimensions.values().next()?;
    if !units.contains(&dimension.get("unit")?.as_str()?)
        || dimension.get("beginRange")?.as_str()? != "0"
        || dimension.get("endRange")?.as_str()? != "Inf"
        || !dimension.get("appliesTo")?.as_array()?.is_empty()
    {
        return None;
    }

    let unit_rates = dimension.get("pricePerUnit")?.as_object()?;
    if unit_rates.len() != 1 || !unit_rates.contains_key("USD") {
        return None;
    }

    let rate = unit_rates.get("USD")?.as_str()?.parse::<Decimal>().ok()?;
    if positive_only && rate <= Decimal::ZERO {
        return None;
    }

    Some((rate, effective))
}

pub(super) fn effective_date(term: &serde_json::Value) -> Option<u64> {
    let date = term.get("effectiveDate")?.as_str()?;
    let timestamp = DateTime::from_str(date, DateTimeFormat::DateTime)
        .ok()?
        .secs();
    u64::try_from(timestamp).ok()
}

pub(super) fn matching_effective_at(
    products: &[String],
    mut matches: impl FnMut(&str) -> bool,
) -> Option<u64> {
    let mut effective = products
        .iter()
        .filter(|raw| matches(raw))
        .filter_map(|raw| {
            let document = serde_json::from_str::<serde_json::Value>(raw).ok()?;
            let terms = document.pointer("/terms/OnDemand")?.as_object()?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs();
            terms
                .values()
                .filter_map(effective_date)
                .filter(|date| *date <= now)
                .max()
        })
        .collect::<Vec<_>>();
    effective.sort_unstable();
    effective.dedup();
    (effective.len() == 1).then(|| effective[0])
}

pub(super) fn unique_rate(mut prices: Vec<(String, Decimal)>) -> Option<Decimal> {
    prices.sort_by(|left, right| left.0.cmp(&right.0));
    prices.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
    (prices.len() == 1).then(|| prices[0].1)
}
