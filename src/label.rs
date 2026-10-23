use std::collections::BTreeMap;

use crate::config::{JobOverrides, RunnerProfilePatch};
use serde::{
    Deserialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};

use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutingRequest {
    pub repository_id: u64,
    pub run_id: u64,
    pub run_attempt: u32,
    pub profile: String,
    pub overrides: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum LabelError {
    #[error("routing label is not canonical")]
    NonCanonical,
    #[error("routing label exceeds 240 ASCII bytes; use a named repository profile")]
    TooLong,
    #[error("invalid routing identity or override")]
    InvalidRequest,
}

impl RoutingRequest {
    /// Serialize the request as its canonical GARM label.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity or override set is invalid, or when
    /// the encoded label exceeds Outrider's conservative 240-byte routing budget.
    pub fn format(&self) -> Result<String, LabelError> {
        if self.repository_id == 0
            || self.run_id == 0
            || self.run_attempt == 0
            || !valid_slug(&self.profile)
        {
            return Err(LabelError::InvalidRequest);
        }

        let allowed = [
            "accounts",
            "image",
            "locations",
            "memory",
            "patch",
            "purchasing",
            "vcpu",
        ];
        if self
            .overrides
            .keys()
            .any(|key| !allowed.contains(&key.as_str()))
            || self
                .overrides
                .iter()
                .any(|(key, value)| !valid_override_value(key, value))
        {
            return Err(LabelError::InvalidRequest);
        }

        let mut token = format!(
            "outrider:v1:{}:{}:{}:{}",
            self.repository_id, self.run_id, self.run_attempt, self.profile
        );
        for (key, value) in &self.overrides {
            token.push(':');
            token.push_str(key);
            token.push('=');
            token.push_str(&encode_value(value));
        }

        if token.len() > 240 {
            return Err(LabelError::TooLong);
        }

        Ok(token)
    }

    /// Convert the label's overrides into the provider's profile patch.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid patch JSON or conflicting override forms.
    pub fn job_overrides(&self) -> Result<JobOverrides, LabelError> {
        let mut profile = if let Some(patch) = self.overrides.get("patch") {
            serde_json::from_str::<RunnerProfilePatch>(patch)
                .map_err(|_| LabelError::InvalidRequest)?
        } else {
            RunnerProfilePatch::default()
        };

        for key in [
            "accounts",
            "image",
            "locations",
            "memory",
            "purchasing",
            "vcpu",
        ] {
            let conflicts = match key {
                "accounts" => profile.accounts.is_some(),
                "image" => profile.image.is_some(),
                "locations" => profile.locations.is_some(),
                "memory" => profile.memory_gib_min.is_some() || profile.memory_gib_max.is_some(),
                "purchasing" => {
                    profile.purchasing.is_some()
                        || profile.purchasing_models.is_some()
                        || profile.purchasing_tiers.is_some()
                }

                "vcpu" => profile.vcpu_min.is_some() || profile.vcpu_max.is_some(),
                _ => false,
            };

            if self.overrides.contains_key(key) && conflicts {
                return Err(LabelError::InvalidRequest);
            }
        }

        if let Some(value) = self.overrides.get("accounts") {
            profile.accounts = Some(value.split(',').map(str::to_owned).collect());
        }

        if let Some(value) = self.overrides.get("image") {
            profile.image = Some(value.clone());
        }

        if let Some(value) = self.overrides.get("locations") {
            profile.locations = Some(value.split(',').map(str::to_owned).collect());
        }

        if let Some(value) = self.overrides.get("memory") {
            profile.memory_gib_min = Some(parse_gib(value)?);
            profile.memory_gib_max = profile.memory_gib_min;
        }

        if let Some(value) = self.overrides.get("purchasing") {
            profile.purchasing = Some(value.clone());
            profile.purchasing_models = Some([value.clone()].into());
            profile.purchasing_tiers = Some(vec![vec![value.clone()]]);
        }

        if let Some(value) = self.overrides.get("vcpu") {
            let count = value
                .parse::<u32>()
                .map_err(|_| LabelError::InvalidRequest)?;
            profile.vcpu_min = Some(count);
            profile.vcpu_max = Some(count);
        }

        Ok(JobOverrides {
            profile,
            ..JobOverrides::default()
        })
    }

    /// Parse and validate a canonical GARM routing label.
    ///
    /// # Errors
    ///
    /// Returns an error for noncanonical, invalid, or oversized labels.
    pub fn parse(token: &str) -> Result<Self, LabelError> {
        if token.len() > 240 {
            return Err(LabelError::TooLong);
        }

        if !token.is_ascii() {
            return Err(LabelError::NonCanonical);
        }

        let mut parts = token.split(':');
        if parts.next() != Some("outrider") || parts.next() != Some("v1") {
            return Err(LabelError::NonCanonical);
        }

        let repository_id = parts
            .next()
            .ok_or(LabelError::NonCanonical)?
            .parse()
            .map_err(|_| LabelError::NonCanonical)?;
        let run_id = parts
            .next()
            .ok_or(LabelError::NonCanonical)?
            .parse()
            .map_err(|_| LabelError::NonCanonical)?;
        let run_attempt = parts
            .next()
            .ok_or(LabelError::NonCanonical)?
            .parse()
            .map_err(|_| LabelError::NonCanonical)?;
        let profile = parts.next().ok_or(LabelError::NonCanonical)?.to_owned();
        let mut overrides = BTreeMap::new();
        for part in parts {
            let (key, value) = part.split_once('=').ok_or(LabelError::NonCanonical)?;
            let decoded = decode_value(value)?;
            if overrides.insert(key.to_owned(), decoded).is_some() {
                return Err(LabelError::NonCanonical);
            }
        }

        let request = Self {
            repository_id,
            run_id,
            run_attempt,
            profile,
            overrides,
        };

        if request.format().map_err(|_| LabelError::NonCanonical)? != token {
            return Err(LabelError::NonCanonical);
        }

        Ok(request)
    }
}

fn valid_override_value(key: &str, value: &str) -> bool {
    if value.is_empty() {
        return false;
    }

    match key {
        "vcpu" => value
            .parse::<u32>()
            .is_ok_and(|count| count > 0 && count.to_string() == value),
        "memory" => parse_gib(value).is_ok(),
        "accounts" | "locations" => {
            let mut previous = None;
            value.split(',').all(|item| {
                let sorted = previous.is_none_or(|last: &str| last < item);
                previous = Some(item);
                sorted && valid_override_slug(item)
            })
        }

        "image" | "purchasing" => valid_override_slug(value),
        "patch" => serde_json::from_str::<StrictJsonValue>(value)
            .ok()
            .filter(|patch| !patch.has_null())
            .and_then(|_| serde_json::from_str::<RunnerProfilePatch>(value).ok())
            .and_then(|patch| canonical_patch_json(&patch))
            .is_some_and(|canonical| canonical == value),
        _ => false,
    }
}

fn parse_gib(value: &str) -> Result<u32, LabelError> {
    let digits = value
        .strip_suffix("gib")
        .ok_or(LabelError::InvalidRequest)?;
    let amount = digits
        .parse::<u32>()
        .map_err(|_| LabelError::InvalidRequest)?;
    if amount == 0 || amount.to_string() != digits {
        return Err(LabelError::InvalidRequest);
    }

    Ok(amount)
}

fn valid_override_slug(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

enum StrictJsonValue {
    Null,
    Boolean,
    Number,
    String,
    Array(Vec<StrictJsonValue>),
    Object(BTreeMap<String, StrictJsonValue>),
}

impl StrictJsonValue {
    fn has_null(&self) -> bool {
        match self {
            Self::Null => true,
            Self::Array(values) => values.iter().any(Self::has_null),
            Self::Object(values) => values.values().any(Self::has_null),
            Self::Boolean | Self::Number | Self::String => false,
        }
    }
}

struct StrictJsonValueVisitor;

impl<'de> Visitor<'de> for StrictJsonValueVisitor {
    type Value = StrictJsonValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value with unique object keys")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictJsonValue::Null)
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(StrictJsonValue::Boolean)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(StrictJsonValue::Number)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(StrictJsonValue::Number)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(StrictJsonValue::Number)
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(StrictJsonValue::String)
    }

    fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
        Ok(StrictJsonValue::String)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element()? {
            values.push(value);
        }

        Ok(StrictJsonValue::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = BTreeMap::new();
        while let Some((key, value)) = map.next_entry()? {
            if values.insert(key, value).is_some() {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
        }

        Ok(StrictJsonValue::Object(values))
    }
}

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonValueVisitor)
    }
}

fn canonical_patch_json(patch: &RunnerProfilePatch) -> Option<String> {
    let mut value = serde_json::to_value(patch).ok()?;
    strip_nulls(&mut value);
    serde_json::to_string(&value).ok()
}

fn strip_nulls(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => values.iter_mut().for_each(strip_nulls),
        serde_json::Value::Object(fields) => {
            fields.retain(|_, field| !field.is_null());
            fields.values_mut().for_each(strip_nulls);
        }

        _ => {}
    }
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn encode_value(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(*byte, b'-' | b'.' | b'_')
        {
            encoded.push(char::from(*byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(b"0123456789abcdef"[usize::from(*byte >> 4)]));
            encoded.push(char::from(b"0123456789abcdef"[usize::from(*byte & 0x0f)]));
        }
    }

    encoded
}

fn decode_value(value: &str) -> Result<String, LabelError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let pair = bytes
                .get(index + 1..index + 3)
                .ok_or(LabelError::NonCanonical)?;
            let high = (pair[0] as char)
                .to_digit(16)
                .ok_or(LabelError::NonCanonical)? as u8;
            let low = (pair[1] as char)
                .to_digit(16)
                .ok_or(LabelError::NonCanonical)? as u8;
            if pair[0].is_ascii_uppercase() || pair[1].is_ascii_uppercase() {
                return Err(LabelError::NonCanonical);
            }

            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }

    String::from_utf8(decoded).map_err(|_| LabelError::NonCanonical)
}
