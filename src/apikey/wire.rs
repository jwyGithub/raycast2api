use serde::{Deserialize, Serialize};

const SCALE: i64 = 1_000_000_000;

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QuotaKind {
    #[default]
    Unlimited,
    Credits,
    Cost,
}

impl QuotaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unlimited => "unlimited",
            Self::Credits => "credits",
            Self::Cost => "cost",
        }
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Disabled,
    NotYetValid,
    Expired,
    QuotaExceeded,
    Active,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Validity {
    Permanent,
    Period,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    Random,
    Custom,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListParams {
    pub page: Option<u32>,
    pub page_size: Option<u32>,
    pub api_key: Option<String>,
    pub status: Option<Status>,
    pub validity: Option<Validity>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Create {
    pub mode: Mode,
    pub api_key: Option<String>,
    pub concurrency_limit: Option<u32>,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub quota_kind: Option<QuotaKind>,
    pub quota_limit: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Update {
    pub id: String,
    pub enabled: Option<bool>,
    pub concurrency_limit: Option<u32>,
    #[serde(default, deserialize_with = "nullable")]
    pub starts_at: Option<Option<i64>>,
    #[serde(default, deserialize_with = "nullable")]
    pub ends_at: Option<Option<i64>>,
    pub quota_kind: Option<QuotaKind>,
    #[serde(default, deserialize_with = "nullable")]
    pub quota_limit: Option<Option<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Id {
    pub id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateModels {
    pub id: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub model_ids: Option<Vec<String>>,
}

fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::deserialize(deserializer).map(Some)
}

#[derive(Serialize)]
pub(crate) struct Item {
    pub id: String,
    pub api_key: String,
    pub api_key_masked: String,
    pub enabled: bool,
    pub status: Status,
    pub concurrency_limit: i64,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub quota_kind: String,
    pub quota_limit: Option<String>,
    pub used_credits: String,
    pub used_cost: String,
    pub model_count: Option<usize>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub(crate) fn parse_units(value: &str) -> Option<i64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || fraction.len() > 9
        || (value.contains('.') && fraction.is_empty())
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.parse::<i64>().ok()?.checked_mul(SCALE)?;
    let fraction = fraction.parse::<i64>().unwrap_or(0) * 10i64.pow(9 - fraction.len() as u32);
    whole.checked_add(fraction)
}

pub(crate) fn usage_units(value: &serde_json::Value) -> Option<i64> {
    let number = value.as_number()?.to_string();
    let (mantissa, exponent) = match number.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i64>().ok()?),
        None => (number.as_str(), 0),
    };
    if mantissa.starts_with('-') {
        return None;
    }
    let whole_digits = mantissa.find('.').unwrap_or(mantissa.len()) as i64;
    let digits: Vec<_> = mantissa.bytes().filter(u8::is_ascii_digit).collect();
    let first = digits
        .iter()
        .position(|&b| b != b'0')
        .unwrap_or(digits.len());
    if first == digits.len() {
        return Some(0);
    }
    let digits = &digits[first..];
    let places = whole_digits
        .saturating_add(exponent)
        .saturating_add(9 - first as i64);
    if places < 0 {
        return Some(0);
    }
    if places > 19 {
        return None;
    }
    let places = places as usize;
    let mut units = 0i64;
    for i in 0..places {
        units = units
            .checked_mul(10)?
            .checked_add(i64::from(digits.get(i).copied().unwrap_or(b'0') - b'0'))?;
    }
    units.checked_add(i64::from(
        digits.get(places).is_some_and(|&digit| digit >= b'5'),
    ))
}

pub(crate) fn format_units(value: i64) -> String {
    let whole = value / SCALE;
    let fraction = value % SCALE;
    if fraction == 0 {
        return whole.to_string();
    }
    format!("{whole}.{fraction:09}")
        .trim_end_matches('0')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_units_are_exact_and_upstream_units_round_without_overflow() {
        assert_eq!(parse_units("9223372036.854775807"), Some(i64::MAX));
        assert_eq!(format_units(i64::MAX), "9223372036.854775807");
        for invalid in [
            "9223372036.854775808",
            "0.1234567890",
            "1e-1",
            "1.",
            "-0",
            " 1",
        ] {
            assert_eq!(parse_units(invalid), None, "{invalid}");
        }
        let usage = |number: &str| usage_units(&serde_json::from_str(number).unwrap());
        assert_eq!(usage("9223372036.854775807"), Some(i64::MAX));
        assert_eq!(usage("9223372036.854775808"), None);
        assert_eq!(usage("9.1105e-3"), Some(9_110_500));
        assert_eq!(usage("4.99e-10"), Some(0));
        assert_eq!(usage("5e-10"), Some(1));
        assert_eq!(usage("-0.001"), None);
        assert_eq!(usage("1e1000"), None);
    }
}
