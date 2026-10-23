use serde_json::json;

use super::billing::{active_term_rate, effective_date};

#[test]
fn effective_date_rejects_invalid_calendar_and_offsets() {
    assert!(
        effective_date(&json!({
            "effectiveDate": "2026-02-30T00:00:00Z"
        }))
        .is_none()
    );
    assert!(
        effective_date(&json!({
            "effectiveDate": "2026-01-01T00:00:00+00:00"
        }))
        .is_none()
    );
}

#[test]
fn future_effective_term_does_not_produce_a_rate() {
    let product = json!({
        "terms": {
            "OnDemand": {
                "offer": {
                    "effectiveDate": "2999-01-01T00:00:00Z",
                    "priceDimensions": {
                        "dimension": {
                            "unit": "Hrs",
                            "beginRange": "0",
                            "endRange": "Inf",
                            "appliesTo": [],
                            "pricePerUnit": {"USD": "0.10"}
                        }
                    }
                }
            }
        }
    });
    assert!(active_term_rate(&product, &["Hrs"], false).is_none());
}
