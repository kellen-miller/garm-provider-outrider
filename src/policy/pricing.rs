use std::collections::BTreeSet;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::PolicyError;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PriceComponent {
    pub hourly_rate: Option<Decimal>,
    pub minimum_commitment_seconds: u64,
    pub source: String,
    pub observed_at_unix_seconds: Option<u64>,
    pub effective_at_unix_seconds: Option<u64>,
    pub included_in_compute: bool,
    pub exclusions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_price_override: Option<AppliedEffectivePrice>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AppliedEffectivePrice {
    pub id: String,
    pub reference: String,
    pub valid_from_unix_seconds: u64,
    pub valid_until_unix_seconds: u64,
    pub original_hourly_rate: Option<Decimal>,
    pub original_source: String,
    pub original_observed_at_unix_seconds: Option<u64>,
    pub original_effective_at_unix_seconds: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ObservationWindow {
    pub start_unix_seconds: u64,
    pub end_unix_seconds: u64,
    pub minimum_samples: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservationAges {
    pub create_seconds: Option<u64>,
    pub latency_seconds: Option<u64>,
    pub interruption_seconds: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PriceQuote {
    pub currency: String,
    pub provider_request_ids: Vec<String>,
    pub compute: PriceComponent,
    pub disk: PriceComponent,
    pub licensing: PriceComponent,
    pub public_address: PriceComponent,
    pub new_host_minimum_commitment_seconds: Option<u64>,
    pub existing_host_remaining_commitment_seconds: Option<u64>,
    pub observed_age_seconds: Option<u64>,
    pub included_charge_names: BTreeSet<String>,
    pub exclusions: Vec<String>,
}

impl PriceQuote {
    /// Set the source observation time and normalize its current age.
    pub fn mark_observed_at(&mut self, observed_at_unix_seconds: u64) {
        for component in [
            &mut self.compute,
            &mut self.disk,
            &mut self.licensing,
            &mut self.public_address,
        ] {
            if !component.included_in_compute && component.hourly_rate.is_some() {
                component.observed_at_unix_seconds = Some(observed_at_unix_seconds);
            }
        }
        self.refresh_observed_age();
    }

    /// Recompute quote age from the earliest required component observation.
    pub fn refresh_observed_age(&mut self) {
        let components = [
            &self.compute,
            &self.disk,
            &self.licensing,
            &self.public_address,
        ];
        let Some(timestamps) = components
            .iter()
            .filter(|component| !component.included_in_compute)
            .map(|component| {
                component
                    .hourly_rate
                    .and(component.observed_at_unix_seconds)
            })
            .collect::<Option<Vec<_>>>()
        else {
            self.observed_age_seconds = None;
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|value| value.as_secs());
        self.observed_age_seconds = now.and_then(|now| {
            if timestamps.iter().any(|observed_at| *observed_at > now) {
                return None;
            }
            timestamps
                .into_iter()
                .min()
                .map(|observed_at| now - observed_at)
        });
    }

    /// Add the available hourly rates for charges billed separately.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported currency, missing rates, negative
    /// rates, or decimal overflow.
    pub fn estimated_hourly(&self) -> Result<Decimal, PolicyError> {
        if self.currency != "USD" {
            return Err(PolicyError::UnsupportedCurrency);
        }

        let mut total = Decimal::ZERO;
        for component in [
            &self.compute,
            &self.disk,
            &self.licensing,
            &self.public_address,
        ] {
            if component.included_in_compute {
                continue;
            }

            let rate = component
                .hourly_rate
                .ok_or(PolicyError::MissingData("price"))?;
            if rate < Decimal::ZERO {
                return Err(PolicyError::NegativePrice);
            }

            total = total.checked_add(rate).ok_or(PolicyError::CostOverflow)?;
        }

        Ok(total)
    }

    /// Estimate the quote over a requested usage duration.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported currency, missing rates or commitment
    /// data, negative rates, or decimal overflow.
    pub fn estimated_total(&self, duration_seconds: u64) -> Result<Decimal, PolicyError> {
        if self.existing_host_remaining_commitment_seconds.is_some() {
            return self.estimated_existing_host_marginal_total(duration_seconds);
        }

        self.estimated_new_host_total(duration_seconds)
    }

    fn estimated_new_host_total(&self, duration_seconds: u64) -> Result<Decimal, PolicyError> {
        if self.currency != "USD" {
            return Err(PolicyError::UnsupportedCurrency);
        }

        let mut total = Decimal::ZERO;
        for (name, component) in [
            ("compute", &self.compute),
            ("disk", &self.disk),
            ("licensing", &self.licensing),
            ("public_address", &self.public_address),
        ] {
            if component.included_in_compute {
                continue;
            }

            let hourly = component
                .hourly_rate
                .ok_or(PolicyError::MissingData("price"))?;
            if hourly < Decimal::ZERO {
                return Err(PolicyError::NegativePrice);
            }

            let host_minimum = if name == "compute" {
                self.new_host_minimum_commitment_seconds.unwrap_or_default()
            } else {
                0
            };

            let seconds = duration_seconds
                .max(component.minimum_commitment_seconds)
                .max(host_minimum);
            let cost = hourly
                .checked_mul(Decimal::from(seconds))
                .and_then(|amount| amount.checked_div(Decimal::from(3600u32)))
                .ok_or(PolicyError::CostOverflow)?;
            total = total.checked_add(cost).ok_or(PolicyError::CostOverflow)?;
        }

        Ok(total)
    }

    /// Estimate incremental charges when the compute host already exists.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported currency, missing rates or commitment
    /// data, negative rates, or decimal overflow.
    pub fn estimated_existing_host_marginal_total(
        &self,
        duration_seconds: u64,
    ) -> Result<Decimal, PolicyError> {
        if self.currency != "USD" {
            return Err(PolicyError::UnsupportedCurrency);
        }

        let remaining = self
            .existing_host_remaining_commitment_seconds
            .ok_or(PolicyError::MissingData("existing host commitment"))?;
        let mut total = Decimal::ZERO;
        for (name, component) in [
            ("compute", &self.compute),
            ("disk", &self.disk),
            ("licensing", &self.licensing),
            ("public_address", &self.public_address),
        ] {
            if component.included_in_compute {
                continue;
            }

            let hourly = component
                .hourly_rate
                .ok_or(PolicyError::MissingData("price"))?;
            if hourly < Decimal::ZERO {
                return Err(PolicyError::NegativePrice);
            }

            let seconds = if name == "compute" {
                duration_seconds.saturating_sub(remaining)
            } else {
                duration_seconds.max(component.minimum_commitment_seconds)
            };

            let cost = hourly
                .checked_mul(Decimal::from(seconds))
                .and_then(|amount| amount.checked_div(Decimal::from(3600u32)))
                .ok_or(PolicyError::CostOverflow)?;
            total = total.checked_add(cost).ok_or(PolicyError::CostOverflow)?;
        }

        Ok(total)
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinancialPriceScope {
    pub cloud: String,
    pub configured_account: String,
    pub native_account: String,
    pub configured_location: String,
    pub native_location: String,
    pub binding_id: String,
    pub machine_type: String,
    pub os: String,
    pub architecture: String,
    pub image_alias: String,
    pub resolved_image: String,
    pub purchasing: String,
    pub vcpu: u32,
    pub memory_gib: rust_decimal::Decimal,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_vendor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_model: Option<String>,
    pub gpu_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_memory_gib: Option<rust_decimal::Decimal>,
    pub boot_disk_class: String,
    pub boot_disk_gib: u32,
    pub boot_disk_iops: u32,
    pub boot_disk_throughput_mib_s: rust_decimal::Decimal,
    pub local_scratch_gib: rust_decimal::Decimal,
    pub launch_facts: ExplanationLaunchFacts,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(
    tag = "cloud",
    content = "facts",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExplanationLaunchFacts {
    Aws {
        availability_zone: String,
        subnet_id: String,
        security_group_ids: Vec<String>,
        map_public_ip_on_launch: bool,
        resolved_image_id: String,
        windows_license_included: bool,
        root_device_name: String,
        minimum_root_volume_gib: u32,
    },
    Azure {
        subscription_id: String,
        native_location: String,
        resolved_image_version: String,
        os_disk_sku: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        public_ip_sku: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        vnet_subnet_id: Option<String>,
        minimum_os_disk_gib: u32,
        image_licensing: crate::config::AzureImageLicensing,
    },
    Gcp {
        project_id: String,
        native_zone: String,
        network_id: String,
        subnetwork_id: String,
        external_ip_access: bool,
        resolved_image_id: String,
        image_minimum_disk_gib: u32,
        bundled_local_ssd_partitions: u32,
        licenses: Vec<ExplanationGcpLicense>,
    },
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExplanationGcpLicense {
    pub license_uri: String,
    pub basis: crate::config::GcpLicenseBasis,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing_sku: Option<String>,
}
