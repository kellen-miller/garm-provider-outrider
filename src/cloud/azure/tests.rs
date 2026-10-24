use super::billing::match_prices;
use super::{AzurePriceRequest, RetailMeter};
use crate::config::AzureImageLicensing;
use rust_decimal::Decimal;

#[test]
fn public_ip_standard_meter_is_priced_independently_of_child_dispatch() {
    let meters: Vec<RetailMeter> = serde_json::from_value(serde_json::json!([
        {
            "serviceName": "Virtual Machines",
            "productName": "Virtual Machines Dsv3 Series",
            "skuName": "D2s v3",
            "armSkuName": "Standard_D2s_v3",
            "meterName": "D2s v3",
            "armRegionName": "eastus",
            "unitOfMeasure": "1 Hour",
            "type": "Consumption",
            "isPrimaryMeterRegion": true,
            "effectiveStartDate": "2026-01-01T00:00:00Z",
            "currencyCode": "USD",
            "tierMinimumUnits": 0,
            "retailPrice": 0.10,
            "unitPrice": 0.10,
            "meterId": "compute-meter"
        },
        {
            "serviceName": "Storage",
            "productName": "Standard HDD Managed Disks",
            "skuName": "S10 LRS",
            "armSkuName": "Standard_LRS",
            "meterName": "S10 LRS Disk",
            "armRegionName": "eastus",
            "unitOfMeasure": "1/Month",
            "type": "Consumption",
            "isPrimaryMeterRegion": true,
            "effectiveStartDate": "2026-01-01T00:00:00Z",
            "currencyCode": "USD",
            "tierMinimumUnits": 0,
            "retailPrice": 5.89,
            "unitPrice": 5.89,
            "meterId": "disk-meter"
        },
        {
            "serviceName": "Virtual Network",
            "productName": "IP Addresses",
            "skuName": "Standard",
            "armSkuName": "Standard",
            "meterName": "Standard IPv4 Static Public IP",
            "armRegionName": "eastus",
            "unitOfMeasure": "1 Hour",
            "type": "Consumption",
            "isPrimaryMeterRegion": false,
            "effectiveStartDate": "2026-01-01T00:00:00Z",
            "currencyCode": "USD",
            "tierMinimumUnits": 0,
            "retailPrice": 0.005,
            "unitPrice": 0.005,
            "meterId": "public-ip-meter"
        }
    ]))
    .unwrap_or_else(|_| panic!("valid Azure Retail meter records"));

    let prices = match_prices(
        &meters,
        AzurePriceRequest {
            sku: "Standard_D2s_v3",
            location: "eastus",
            os: "linux",
            purchasing: "on_demand",
            disk_sku: "Standard_LRS",
            disk_gib: 127,
            licensing: AzureImageLicensing::NoAdditionalSoftwareCharge,
            licensing_source: "operator verified image pricing documentation",
            public_requested: true,
        },
    )
    .unwrap_or_else(|| panic!("standard public IP meter matches independently"));

    assert_eq!(
        prices
            .public_ip
            .unwrap_or_else(|| panic!("public IP meter"))
            .unit_price,
        Decimal::new(5, 3)
    );
}
