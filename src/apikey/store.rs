use std::collections::{BTreeSet, HashMap};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU32, Ordering},
};

use axum::http::StatusCode;
use diesel::{connection::SimpleConnection, prelude::*, sqlite::SqliteConnection};

use super::{KeyId, Permit, invalid_key, model_not_found, wire::*};
use crate::{
    core::page::{Page, page_params},
    error::GatewayError,
    storage::{
        db::{Db, blocking},
        schema::api_keys as k,
    },
    util::{fill_random, mask_api_key, new_ulid, unix_secs},
};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS api_keys (
    id TEXT PRIMARY KEY NOT NULL,
    api_key TEXT NOT NULL UNIQUE,
    enabled INTEGER NOT NULL,
    concurrency_limit INTEGER NOT NULL CHECK(concurrency_limit > 0),
    starts_at INTEGER,
    ends_at INTEGER,
    quota_kind TEXT NOT NULL CHECK(quota_kind IN ('unlimited', 'credits', 'cost')),
    quota_limit INTEGER,
    used_credits INTEGER NOT NULL,
    used_cost INTEGER NOT NULL,
    model_ids TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
)";

#[derive(Clone, Queryable, Selectable, Identifiable, Insertable, AsChangeset)]
#[diesel(table_name = k, treat_none_as_null = true)]
struct Row {
    id: String,
    api_key: String,
    enabled: bool,
    concurrency_limit: i64,
    starts_at: Option<i64>,
    ends_at: Option<i64>,
    quota_kind: String,
    quota_limit: Option<i64>,
    used_credits: i64,
    used_cost: i64,
    model_ids: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Clone)]
struct Entry {
    row: Row,
    models: Option<BTreeSet<String>>,
    in_flight: Arc<AtomicU32>,
}

impl Entry {
    fn status(&self, now: i64) -> Status {
        let row = &self.row;
        if !row.enabled {
            Status::Disabled
        } else if row.starts_at.is_some_and(|start| now < start) {
            Status::NotYetValid
        } else if row.ends_at.is_some_and(|end| now >= end) {
            Status::Expired
        } else if row.quota_limit.is_some_and(|limit| {
            let used = if row.quota_kind == "credits" {
                row.used_credits
            } else {
                row.used_cost
            };
            used >= limit
        }) {
            Status::QuotaExceeded
        } else {
            Status::Active
        }
    }

    fn check(&self) -> Result<(), GatewayError> {
        use Status::*;
        let (status, code, message) = match self.status(unix_secs() as i64) {
            Active => return Ok(()),
            Disabled => (
                StatusCode::UNAUTHORIZED,
                "api_key_disabled",
                "API key is disabled.",
            ),
            NotYetValid => (
                StatusCode::UNAUTHORIZED,
                "api_key_not_yet_valid",
                "API key is not yet valid.",
            ),
            Expired => (
                StatusCode::UNAUTHORIZED,
                "api_key_expired",
                "API key has expired.",
            ),
            QuotaExceeded => (
                StatusCode::FORBIDDEN,
                "insufficient_quota",
                "API key quota has been exhausted.",
            ),
        };
        Err(GatewayError::Admission {
            status,
            code,
            message: message.into(),
        })
    }

    fn item(&self, now: i64) -> Item {
        let r = &self.row;
        let key = &r.api_key;
        Item {
            id: r.id.clone(),
            api_key: key.clone(),
            api_key_masked: mask_api_key(key),
            enabled: r.enabled,
            status: self.status(now),
            concurrency_limit: r.concurrency_limit,
            starts_at: r.starts_at,
            ends_at: r.ends_at,
            quota_kind: r.quota_kind.clone(),
            quota_limit: r.quota_limit.map(format_units),
            used_credits: format_units(r.used_credits),
            used_cost: format_units(r.used_cost),
            model_count: self.models.as_ref().map(BTreeSet::len),
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

pub(crate) struct ApiKeyStore {
    db: Db,
    cache: RwLock<HashMap<String, Entry>>,
}

impl ApiKeyStore {
    pub fn new(db: Db, global: &str) -> Result<Self, String> {
        let rows = blocking(|| {
            let mut conn = db.lock();
            conn.batch_execute(SCHEMA)?;
            k::table.select(Row::as_select()).load::<Row>(&mut *conn)
        })
        .map_err(|error| format!("could not load managed API keys: {error}"))?;
        let mut cache = HashMap::new();
        for row in rows {
            if row.api_key == global {
                return Err(format!(
                    "configured api_key conflicts with managed API key ID {}",
                    row.id
                ));
            }
            let models = row
                .model_ids
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|_| {
                    format!("invalid model whitelist for managed API key ID {}", row.id)
                })?;
            cache.insert(
                row.id.clone(),
                Entry {
                    row,
                    models,
                    in_flight: Arc::default(),
                },
            );
        }
        Ok(Self {
            db,
            cache: RwLock::new(cache),
        })
    }

    pub fn verify(&self, credential: &str) -> Result<KeyId, GatewayError> {
        let cache = self.cache.read().unwrap();
        let entry = cache
            .values()
            .find(|entry| entry.row.api_key == credential)
            .ok_or_else(invalid_key)?;
        entry.check()?;
        Ok(KeyId(entry.row.id.clone()))
    }

    pub fn contains(&self, credential: &str) -> bool {
        self.cache
            .read()
            .unwrap()
            .values()
            .any(|entry| entry.row.api_key == credential)
    }

    pub fn admit(
        &self,
        id: &KeyId,
        target: &str,
        requested: &str,
    ) -> Result<(Permit, bool), GatewayError> {
        let cache = self.cache.read().unwrap();
        let entry = cache.get(&id.0).ok_or_else(invalid_key)?;
        entry.check()?;
        if entry
            .models
            .as_ref()
            .is_some_and(|models| !models.contains(target))
        {
            return Err(model_not_found(requested));
        }
        entry
            .in_flight
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (i64::from(current) < entry.row.concurrency_limit).then(|| current + 1)
            })
            .map_err(|_| GatewayError::Admission {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: "api_key_concurrency_limit",
                message: "API key concurrency limit reached.".into(),
            })?;
        Ok((
            Permit(entry.in_flight.clone()),
            entry.row.quota_limit.is_some(),
        ))
    }

    pub fn permitted_models(&self, id: &KeyId) -> Result<Option<BTreeSet<String>>, GatewayError> {
        let cache = self.cache.read().unwrap();
        let entry = cache.get(&id.0).ok_or_else(invalid_key)?;
        entry.check()?;
        Ok(entry.models.clone())
    }

    pub fn models(&self, id: &str) -> Result<Option<BTreeSet<String>>, GatewayError> {
        let cache = self.cache.read().unwrap();
        Ok(cache.get(id).ok_or_else(not_found)?.models.clone())
    }

    pub fn list(&self, params: ListParams) -> Result<Page<Item>, GatewayError> {
        let (page, size) = page_params(params.page, params.page_size)?;
        let now = unix_secs() as i64;
        let cache = self.cache.read().unwrap();
        let mut entries: Vec<_> = cache
            .values()
            .filter(|entry| {
                params
                    .api_key
                    .as_ref()
                    .is_none_or(|key| *key == entry.row.api_key)
                    && params
                        .status
                        .is_none_or(|status| entry.status(now) == status)
                    && match params.validity {
                        None => true,
                        Some(Validity::Permanent) => entry.row.ends_at.is_none(),
                        Some(Validity::Period) => entry.row.ends_at.is_some(),
                    }
            })
            .collect();
        entries
            .sort_unstable_by_key(|entry| std::cmp::Reverse((entry.row.created_at, &entry.row.id)));
        let total = entries.len() as u64;
        let items = entries
            .into_iter()
            .skip(usize::try_from(u64::from(page - 1) * u64::from(size)).unwrap_or(usize::MAX))
            .take(size as usize)
            .map(|entry| entry.item(now))
            .collect();
        Ok(Page::new(items, total, page, size))
    }

    pub fn create(&self, params: Create, global: &str) -> Result<Item, GatewayError> {
        let api_key = match params.mode {
            Mode::Random if params.api_key.is_none() => {
                let mut bytes = [0; 32];
                fill_random(&mut bytes)
                    .map_err(|_| GatewayError::Internal("无法生成安全的 API Key。".into()))?;
                format!("sk-{}", hex::encode(bytes))
            }
            Mode::Random => return Err(bad("随机模式不能指定 API Key。")),
            Mode::Custom => params
                .api_key
                .filter(|key| {
                    (16..=128).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_graphic())
                })
                .ok_or_else(|| {
                    bad("自定义 API Key 须为 16 至 128 个不含空白的可见 ASCII 字符。")
                })?,
        };
        if api_key == global {
            return Err(conflict());
        }
        let now = unix_secs() as i64;
        let mut row = Row {
            id: new_ulid(),
            api_key,
            enabled: true,
            concurrency_limit: i64::from(params.concurrency_limit.unwrap_or(1)),
            starts_at: params.starts_at,
            ends_at: params.ends_at,
            quota_kind: params.quota_kind.unwrap_or_default().as_str().into(),
            quota_limit: None,
            used_credits: 0,
            used_cost: 0,
            model_ids: None,
            created_at: now,
            updated_at: now,
        };
        if let Some(amount) = params.quota_limit {
            row.quota_limit = Some(limit(&amount)?);
        }
        validate(&mut row)?;
        let entry = Entry {
            row,
            models: None,
            in_flight: Arc::default(),
        };
        blocking(|| {
            let mut conn = self.db.lock();
            if self
                .cache
                .read()
                .unwrap()
                .values()
                .any(|other| other.row.api_key == entry.row.api_key)
            {
                return Err(conflict());
            }
            diesel::insert_into(k::table)
                .values(&entry.row)
                .execute(&mut *conn)
                .map_err(db_error)?;
            let item = entry.item(now);
            self.cache
                .write()
                .unwrap()
                .insert(entry.row.id.clone(), entry);
            Ok(item)
        })
    }

    pub fn update(&self, change: Update) -> Result<Item, GatewayError> {
        self.edit(&change.id, |entry| {
            let row = &mut entry.row;
            if let Some(value) = change.enabled {
                row.enabled = value;
            }
            if let Some(value) = change.concurrency_limit {
                row.concurrency_limit = value.into();
            }
            if let Some(value) = change.starts_at {
                row.starts_at = value;
            }
            if let Some(value) = change.ends_at {
                row.ends_at = value;
            }
            if let Some(kind) = change.quota_kind {
                row.quota_kind = kind.as_str().into();
            }
            if let Some(value) = change.quota_limit {
                row.quota_limit = value.as_deref().map(limit).transpose()?;
            }
            validate(row)
        })
    }

    pub fn update_models(
        &self,
        id: &str,
        models: Option<Vec<String>>,
        available: &BTreeSet<String>,
    ) -> Result<Item, GatewayError> {
        self.edit(id, |entry| {
            let models = models.map(|ids| ids.into_iter().collect::<BTreeSet<_>>());
            if let Some(ids) = &models {
                if ids.is_empty() {
                    return Err(bad("模型白名单不能为空。"));
                }
                if ids.iter().any(|model| {
                    !available.contains(model)
                        && entry.models.as_ref().is_none_or(|old| !old.contains(model))
                }) {
                    return Err(bad("不能添加目录中不存在的模型。"));
                }
            }
            entry.row.model_ids = models.as_ref().map(serde_json::to_string).transpose()?;
            entry.models = models;
            Ok(())
        })
    }

    pub fn reset_usage(&self, id: &str) -> Result<Item, GatewayError> {
        self.edit(id, |entry| {
            entry.row.used_credits = 0;
            entry.row.used_cost = 0;
            Ok(())
        })
    }

    fn edit(
        &self,
        id: &str,
        change: impl FnOnce(&mut Entry) -> Result<(), GatewayError>,
    ) -> Result<Item, GatewayError> {
        blocking(|| {
            let mut conn = self.db.lock();
            let mut entry = self
                .cache
                .read()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(not_found)?;
            change(&mut entry)?;
            let now = unix_secs() as i64;
            entry.row.updated_at = now;
            if diesel::update(k::table.find(id))
                .set(&entry.row)
                .execute(&mut *conn)
                .map_err(db_error)?
                == 0
            {
                return Err(not_found());
            }
            let item = entry.item(now);
            self.cache.write().unwrap().insert(id.into(), entry);
            Ok(item)
        })
    }

    pub fn delete(&self, id: &str) -> Result<(), GatewayError> {
        blocking(|| {
            let mut conn = self.db.lock();
            if diesel::delete(k::table.find(id))
                .execute(&mut *conn)
                .map_err(db_error)?
                == 0
            {
                return Err(not_found());
            }
            self.cache.write().unwrap().remove(id);
            Ok(())
        })
    }

    pub fn add_usage(
        conn: &mut SqliteConnection,
        id: &KeyId,
        credits: i64,
        cost: i64,
    ) -> QueryResult<Option<(i64, i64)>> {
        let target = k::table.find(&id.0);
        let Some((used_credits, used_cost)) = target
            .select((k::used_credits, k::used_cost))
            .first::<(i64, i64)>(conn)
            .optional()?
        else {
            return Ok(None);
        };
        let totals = (
            used_credits.saturating_add(credits),
            used_cost.saturating_add(cost),
        );
        diesel::update(target)
            .set((k::used_credits.eq(totals.0), k::used_cost.eq(totals.1)))
            .execute(conn)?;
        Ok(Some(totals))
    }

    /// Called after commit while the shared SQLite connection is still locked.
    pub fn publish_usage(&self, id: &KeyId, (credits, cost): (i64, i64)) {
        if let Some(entry) = self.cache.write().unwrap().get_mut(&id.0) {
            entry.row.used_credits = credits;
            entry.row.used_cost = cost;
        }
    }
}

fn validate(row: &mut Row) -> Result<(), GatewayError> {
    if row.concurrency_limit == 0 {
        return Err(bad("并发上限必须是正整数。"));
    }
    // The console uses four-digit years in its fixed UTC+8 calendar.
    const MAX_TIME: i64 = 253_402_271_999;
    if row
        .starts_at
        .into_iter()
        .chain(row.ends_at)
        .any(|time| !(0..=MAX_TIME).contains(&time))
        || row
            .ends_at
            .is_some_and(|end| row.starts_at.is_none_or(|start| start >= end))
    {
        return Err(bad(
            "有效期须使用 UTC 秒；期限模式须指定早于结束时间的开始时间。",
        ));
    }
    if row.quota_kind == "unlimited" {
        row.quota_limit = None;
    } else if row.quota_limit.is_none() {
        return Err(bad("限额模式须设置大于零的总额度。"));
    }
    Ok(())
}

fn limit(value: &str) -> Result<i64, GatewayError> {
    parse_units(value)
        .filter(|value| *value > 0)
        .ok_or_else(|| bad("总额度须为大于零且可保存的十进制数，最多九位小数。"))
}

fn bad(message: &str) -> GatewayError {
    GatewayError::BadRequest(message.into())
}
fn not_found() -> GatewayError {
    GatewayError::NotFound("托管 API Key 不存在。".into())
}
fn conflict() -> GatewayError {
    GatewayError::Conflict("该 API Key 已存在。".into())
}
fn db_error(error: diesel::result::Error) -> GatewayError {
    tracing::error!(%error, "managed API key storage operation failed");
    GatewayError::Internal("无法保存托管 API Key。".into())
}
