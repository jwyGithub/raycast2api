//! Statistics query contracts. Stored timestamps are UTC seconds; JSON uses Asia/Shanghai.
use diesel::prelude::*;
use serde::{Deserialize, Serialize, Serializer};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    pub page: Option<u32>,
    pub page_size: Option<u32>,
    pub requested_at_from: Option<i64>,
    pub requested_at_to: Option<i64>,
    pub request_id: Option<String>,
    pub account_email: Option<String>,
    pub requested_model: Option<String>,
    pub upstream_model: Option<String>,
    pub api_key: Option<String>,
    pub call_type: Option<String>,
    pub status: Option<String>,
}

impl ListParams {
    pub fn pagination(&self) -> Result<(u32, u32), String> {
        let (page, size) = crate::core::page::page_params(self.page, self.page_size)
            .map_err(|error| error.to_string())?;
        if self.requested_at_from.is_some_and(|from| from < 0)
            || self.requested_at_to.is_some_and(|to| to < 0)
            || matches!((self.requested_at_from, self.requested_at_to), (Some(from), Some(to)) if from > to)
        {
            return Err(
                "requested_at range must be non-negative UTC seconds with from <= to".into(),
            );
        }
        Ok((page, size))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetailParams {
    pub id: String,
}

#[derive(Debug, Queryable, Selectable, Serialize)]
#[diesel(table_name = crate::storage::schema::statistics)]
#[diesel(check_for_backend(diesel::sqlite::Sqlite))]
pub struct Statistic {
    pub id: String,
    pub request_id: String,
    pub api_key: Option<String>,
    pub api_key_masked: Option<String>,
    #[serde(serialize_with = "serialize_time")]
    pub requested_at: i64,
    #[serde(serialize_with = "serialize_time")]
    pub started_at: i64,
    #[serde(serialize_with = "serialize_optional_time")]
    pub finished_at: Option<i64>,
    pub request_ip: Option<String>,
    pub account_email: String,
    pub requested_model: String,
    pub upstream_provider: String,
    pub upstream_model: String,
    pub requested_effort: Option<String>,
    #[serde(serialize_with = "serialize_json")]
    pub requested_thinking_json: Option<String>,
    pub upstream_effort: Option<String>,
    pub call_type: String,
    pub status: String,
    pub http_status: Option<i32>,
    pub error_message: Option<String>,
    #[serde(serialize_with = "serialize_json")]
    pub usage_json: Option<String>,
}

fn shanghai_time(seconds: i64) -> Result<String, time::error::ComponentRange> {
    let t = time::OffsetDateTime::from_unix_timestamp(seconds)?
        .to_offset(time::UtcOffset::from_hms(8, 0, 0).unwrap());
    Ok(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    ))
}

fn serialize_time<S: Serializer>(seconds: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    shanghai_time(*seconds)
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}

fn serialize_optional_time<S: Serializer>(
    seconds: &Option<i64>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    seconds
        .map(shanghai_time)
        .transpose()
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}

fn serialize_json<S: Serializer>(json: &Option<String>, serializer: S) -> Result<S::Ok, S::Error> {
    json.as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
        .transpose()
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}
