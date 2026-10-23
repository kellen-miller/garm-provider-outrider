use super::billing::sku_hourly;
use super::discovery::machine_type_usable;
use super::{
    BillingAggregationInfo, BillingCategory, BillingExpression, BillingMoney, BillingPricingInfo,
    BillingSku, BillingTier, GcpDeprecated, GcpMachineType,
};
use rust_decimal::Decimal;

fn sku(pricing_info: Vec<BillingPricingInfo>) -> BillingSku {
    BillingSku {
        name: "services/6F81-5844-456A/skus/test".to_owned(),
        description: "contract SKU".to_owned(),
        category: BillingCategory {
            resource_family: "Compute".to_owned(),
            resource_group: "CPU".to_owned(),
            usage_type: "OnDemand".to_owned(),
        },
        service_regions: vec!["us-central1".to_owned()],
        geo_taxonomy: None,
        pricing_info,
    }
}

fn pricing_info(
    effective_time: &str,
    usage_unit: &str,
    tiers: Vec<BillingTier>,
) -> BillingPricingInfo {
    BillingPricingInfo {
        effective_time: effective_time.to_owned(),
        pricing_expression: BillingExpression {
            usage_unit: usage_unit.to_owned(),
            tiered_rates: tiers,
        },
        aggregation_info: None,
    }
}

fn tier(start_usage_amount: i64, units: &str, nanos: i32) -> BillingTier {
    BillingTier {
        start_usage_amount: Decimal::from(start_usage_amount),
        unit_price: BillingMoney {
            currency_code: "USD".to_owned(),
            units: units.to_owned(),
            nanos,
        },
    }
}

#[test]
fn gcp_billing_current_term_and_display_quantity() {
    let info: BillingPricingInfo = serde_json::from_str(
        r#"{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"GiBy.h","displayQuantity":100,"tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":5000000}}]},"aggregationInfo":{"aggregationLevel":"ACCOUNT","aggregationInterval":"1h","aggregationCount":1}}"#,
    )
    .unwrap_or_else(|_| panic!("valid Cloud Billing expression"));
    let sku = sku(vec![
        info,
        pricing_info("2030-01-01T00:00:00Z", "GiBy.h", vec![tier(0, "7", 0)]),
    ]);
    let (rate, effective) =
        sku_hourly(&sku).unwrap_or_else(|error| panic!("billing rate: {error}"));
    assert_eq!(rate, Decimal::new(5, 3));
    assert_eq!(
        sku.pricing_info[0]
            .aggregation_info
            .as_ref()
            .and_then(|value| value.aggregation_count),
        Some(1)
    );
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .timestamp() as u64,
        effective
    );
}

#[test]
fn gcp_billing_rejects_progressive_tiers() {
    let sku = sku(vec![pricing_info(
        "2025-01-01T00:00:00Z",
        "h",
        vec![tier(0, "0", 10_000_000), tier(100, "0", 5_000_000)],
    )]);
    assert!(sku_hourly(&sku).is_err());
}

#[test]
fn gcp_billing_rejects_conflicting_effective_units() {
    let sku = sku(vec![
        pricing_info("2025-01-01T00:00:00Z", "h", vec![tier(0, "0", 10_000_000)]),
        pricing_info(
            "2025-01-01T00:00:00Z",
            "GiBy.mo",
            vec![tier(0, "0", 10_000_000)],
        ),
    ]);
    assert!(sku_hourly(&sku).is_err());
}

#[test]
fn gcp_billing_rejects_conflicting_effective_aggregation() {
    let first = pricing_info("2025-01-01T00:00:00Z", "h", vec![tier(0, "0", 10_000_000)]);
    let mut second = first.clone();
    second.aggregation_info = Some(BillingAggregationInfo {
        aggregation_level: Some("ACCOUNT".to_owned()),
        aggregation_interval: Some("1h".to_owned()),
        aggregation_count: Some(1),
    });
    assert!(sku_hourly(&sku(vec![first, second])).is_err());
}

#[test]
fn gcp_machine_types_require_active_lifecycle() {
    let active = GcpMachineType {
        name: "n2-standard-4".to_owned(),
        guest_cpus: 4,
        memory_mb: 16384,
        architecture: Some("X86_64".to_owned()),
        is_shared_cpu: false,
        accelerators: Vec::new(),
        bundled_local_ssds: Vec::new(),
        deprecated: None,
    };
    assert!(machine_type_usable(&active));
    for state in ["ACTIVE", "OBSOLETE", "DELETED", "UNKNOWN"] {
        let mut machine = active.clone();
        machine.deprecated = Some(GcpDeprecated {
            state: state.to_owned(),
        });
        assert_eq!(
            machine_type_usable(&machine),
            state == "ACTIVE",
            "state={state}"
        );
    }
}
