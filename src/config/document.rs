use std::collections::BTreeSet;

use toml_edit::{DocumentMut, TableLike};

use super::{ConfigError, ConfigPatch};

impl ConfigPatch {
    /// Parse and validate an Outrider TOML configuration.
    ///
    /// # Errors
    /// Returns an error for malformed TOML, unknown policy fields, or an
    /// unsupported version.
    pub fn from_toml(toml: &str) -> Result<Self, ConfigError> {
        let document = toml
            .parse::<DocumentMut>()
            .map_err(|error| ConfigError::Toml(error.to_string()))?;
        reject_unknown_policy_fields(&document)?;
        let patch: Self = toml_edit::de::from_document(document)
            .map_err(|error| ConfigError::Toml(error.to_string()))?;
        patch.validate_version()?;
        Ok(patch)
    }

    pub(super) fn validate_version(&self) -> Result<(), ConfigError> {
        if self.version == Some(1) {
            self.validate_admission_settings()?;
            self.validate_effective_prices()
        } else {
            Err(ConfigError::UnsupportedVersion)
        }
    }

    /// Validate administrator allocation admission without requiring an open window.
    ///
    /// # Errors
    /// Returns an error for a relative state path or a zero maximum window.
    pub fn validate_admission_settings(&self) -> Result<(), ConfigError> {
        if self.admission.as_ref().is_some_and(|settings| {
            !settings.state_file.is_absolute() || settings.max_window_seconds == 0
        }) {
            return Err(ConfigError::InvalidProfile(
                "admission requires an absolute state_file and positive max_window_seconds".into(),
            ));
        }

        Ok(())
    }

    fn validate_effective_prices(&self) -> Result<(), ConfigError> {
        let Some(prices) = &self.effective_prices else {
            return Ok(());
        };
        let mut ids = BTreeSet::new();
        for price in prices {
            let id = price.id.trim();
            if id.is_empty()
                || id != price.id
                || id.len() > 128
                || id
                    .as_bytes()
                    .first()
                    .is_none_or(|byte| !byte.is_ascii_alphanumeric())
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
                || !ids.insert(id.to_owned())
            {
                return Err(ConfigError::InvalidProfile(
                    "effective price IDs must be unique bounded identifiers".to_owned(),
                ));
            }

            if price.currency != "USD"
                || price.hourly_rate < rust_decimal::Decimal::ZERO
                || price.valid_from_unix_seconds >= price.valid_until_unix_seconds
                || !matches!(
                    (price.scope.cloud.as_str(), &price.scope.launch_facts),
                    ("aws", crate::policy::ExplanationLaunchFacts::Aws { .. })
                        | ("azure", crate::policy::ExplanationLaunchFacts::Azure { .. })
                        | ("gcp", crate::policy::ExplanationLaunchFacts::Gcp { .. })
                )
            {
                return Err(ConfigError::InvalidProfile(format!(
                    "effective price {} has invalid currency, rate, or validity interval",
                    price.id
                )));
            }

            for evidence in [&price.source, &price.reference] {
                if evidence.trim().is_empty()
                    || evidence.chars().count() > 256
                    || evidence.chars().any(char::is_control)
                {
                    return Err(ConfigError::InvalidProfile(format!(
                        "effective price {} has invalid evidence metadata",
                        price.id
                    )));
                }
            }
        }

        for (index, price) in prices.iter().enumerate() {
            if prices[index + 1..].iter().any(|other| {
                price.scope == other.scope
                    && price.component == other.component
                    && price.valid_from_unix_seconds < other.valid_until_unix_seconds
                    && other.valid_from_unix_seconds < price.valid_until_unix_seconds
            }) {
                return Err(ConfigError::InvalidProfile(format!(
                    "effective price {} overlaps another price for the same scope and component",
                    price.id
                )));
            }
        }

        Ok(())
    }
}

fn reject_unknown_policy_fields(document: &DocumentMut) -> Result<(), ConfigError> {
    let Some(policies) = document
        .get("policies")
        .and_then(toml_edit::Item::as_table_like)
    else {
        return Ok(());
    };

    for (name, definition) in policies.iter() {
        let Some(definition) = definition.as_table_like() else {
            continue;
        };

        for (field, value) in definition.iter() {
            if !["strategy", "missing_data"].contains(&field) {
                return Err(ConfigError::UnknownField(format!(
                    "policies.{name}.{field}"
                )));
            }

            let Some(nested) = value.as_table_like() else {
                continue;
            };

            let allowed = policy_variant_fields(field, nested);
            if let Some((nested_field, _)) = nested
                .iter()
                .find(|(nested_field, _)| !allowed.contains(nested_field))
            {
                return Err(ConfigError::UnknownField(format!(
                    "policies.{name}.{field}.{nested_field}"
                )));
            }
        }
    }

    Ok(())
}

fn policy_variant_fields(field: &str, nested: &dyn TableLike) -> &'static [&'static str] {
    let tag = if field == "strategy" {
        "strategy"
    } else {
        "action"
    };

    let tag_value = nested.get(tag).and_then(toml_edit::Item::as_str);
    match (field, tag_value) {
        ("strategy", Some("economy")) => &["strategy", "max_price_age_seconds"],
        ("strategy", Some("continuity")) => {
            &["strategy", "max_observation_age_seconds", "min_samples"]
        }

        ("strategy", Some("weighted")) => &[
            "strategy",
            "cost_weight",
            "failure_weight",
            "interruption_weight",
            "latency_weight",
            "cost_scale",
            "interruption_scale",
            "latency_scale",
            "min_samples",
            "max_observation_age_seconds",
            "max_price_age_seconds",
        ],
        ("strategy", Some("ordered")) => &["strategy", "identities"],
        ("missing_data", Some("reject")) => &["action"],
        ("missing_data", Some("fallback")) => &["action", "policy"],
        _ => &[],
    }
}
