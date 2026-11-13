//! 请求日志存储:request_logs 表的写入、查询与过期清理。
use crate::{
    core::page::Page,
    db::Db,
    log::wire::{LogDetail, LogEntry},
    schema::request_logs,
    util::unix_secs,
};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;

pub const RETENTION_SECS: i64 = 7 * 24 * 60 * 60;
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS request_logs (id INTEGER PRIMARY KEY AUTOINCREMENT, request_id TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL, method TEXT, path TEXT, model TEXT, upstream_model TEXT, inbound_json TEXT, upstream_json TEXT, response_text TEXT); CREATE INDEX IF NOT EXISTS idx_request_logs_created ON request_logs(created_at);";
/// 旧库补列;新库或已迁移的库会报 duplicate column,忽略即可。
const ADD_MODEL: &str = "ALTER TABLE request_logs ADD COLUMN model TEXT";
const ADD_UPSTREAM_MODEL: &str = "ALTER TABLE request_logs ADD COLUMN upstream_model TEXT";

#[derive(Queryable, Selectable)]
#[diesel(table_name = request_logs)]
#[diesel(check_for_backend(diesel::sqlite::Sqlite))]
struct StoredLog {
    request_id: String,
    created_at: i64,
    method: Option<String>,
    path: Option<String>,
    model: Option<String>,
    upstream_model: Option<String>,
    inbound_json: Option<String>,
    upstream_json: Option<String>,
    response_text: Option<String>,
}
#[derive(Insertable)]
#[diesel(table_name = request_logs)]
struct NewLog<'a> {
    request_id: &'a str,
    created_at: i64,
    method: Option<&'a str>,
    path: Option<&'a str>,
    model: Option<&'a str>,
    inbound_json: Option<&'a str>,
}
impl StoredLog {
    fn detail(self) -> LogDetail {
        LogDetail {
            request_id: self.request_id,
            created_at: self.created_at,
            method: self.method,
            path: self.path,
            model: self.model,
            upstream_model: self.upstream_model,
            inbound_json: self.inbound_json,
            upstream_json: self.upstream_json,
            response_text: self.response_text,
        }
    }
}
pub struct RequestLogStore {
    pub(crate) db: Db,
}
impl RequestLogStore {
    pub fn new(db: Db) -> Self {
        crate::db::blocking(|| {
            if let Err(e) = db.lock().batch_execute(SCHEMA) {
                tracing::error!("request log schema: {e}");
            }
            let _ = db.lock().batch_execute(ADD_MODEL);
            let _ = db.lock().batch_execute(ADD_UPSTREAM_MODEL);
            let store = Self { db };
            if store.cleanup_expired() > 0 {
                store.vacuum();
            }
            store
        })
    }
    pub fn count(&self) -> Result<u64, String> {
        crate::db::blocking(|| {
            let mut c = self.db.lock();
            request_logs::table
                .count()
                .get_result::<i64>(&mut *c)
                .map(|n| n as u64)
                .map_err(|e| e.to_string())
        })
    }
    pub fn insert_request(&self, id: &str, method: &str, path: &str, body: &serde_json::Value) {
        crate::db::blocking(|| {
            let model = body.get("model").and_then(|m| m.as_str());
            let body = serde_json::to_string_pretty(body).unwrap_or_default();
            let mut c = self.db.lock();
            if let Err(e) = diesel::insert_into(request_logs::table)
                .values(NewLog {
                    request_id: id,
                    created_at: unix_secs() as i64,
                    method: Some(method),
                    path: Some(path),
                    model,
                    inbound_json: Some(&body),
                })
                .on_conflict(request_logs::request_id)
                .do_nothing()
                .execute(&mut *c)
            {
                tracing::warn!("request log insert ({id}): {e}");
            }
        })
    }
    pub fn insert_request_without_body(&self, id: &str, method: &str, path: &str) {
        crate::db::blocking(|| {
            let mut c = self.db.lock();
            if let Err(e) = diesel::insert_into(request_logs::table)
                .values(NewLog {
                    request_id: id,
                    created_at: unix_secs() as i64,
                    method: Some(method),
                    path: Some(path),
                    model: None,
                    inbound_json: None,
                })
                .on_conflict(request_logs::request_id)
                .do_nothing()
                .execute(&mut *c)
            {
                tracing::warn!("request log insert ({id}): {e}");
            }
        })
    }
    pub fn update_upstream(&self, id: &str, body: &serde_json::Value) {
        crate::db::blocking(|| {
            // 从最终发送的请求体提取,避免记录成映射前的客户端模型。
            let upstream_model = body.get("model").and_then(|model| model.as_str());
            let body = serde_json::to_string_pretty(body).unwrap_or_default();
            let mut c = self.db.lock();
            if let Err(e) =
                diesel::update(request_logs::table.filter(request_logs::request_id.eq(id)))
                    .set((
                        request_logs::upstream_json.eq(Some(body)),
                        request_logs::upstream_model.eq(upstream_model),
                    ))
                    .execute(&mut *c)
            {
                tracing::warn!("request log upstream ({id}): {e}");
            }
        })
    }
    pub fn update_response(&self, id: &str, body: &str) {
        crate::db::blocking(|| {
            let mut c = self.db.lock();
            if let Err(e) =
                diesel::update(request_logs::table.filter(request_logs::request_id.eq(id)))
                    .set(request_logs::response_text.eq(Some(body)))
                    .execute(&mut *c)
            {
                tracing::warn!("request log response ({id}): {e}");
            }
        })
    }
    pub fn list(
        &self,
        (page, page_size): (u32, u32),
        created_at: Option<i64>,
        request_id: Option<&str>,
    ) -> Result<Page<LogEntry>, String> {
        crate::db::blocking(|| {
            if page == 0 || page_size == 0 {
                return Err("page and page_size must be positive".into());
            }
            let mut c = self.db.lock();
            let mut query = request_logs::table.into_boxed::<diesel::sqlite::Sqlite>();
            let mut count_query = request_logs::table.into_boxed::<diesel::sqlite::Sqlite>();
            if let Some(created_at) = created_at {
                query = query.filter(request_logs::created_at.eq(created_at));
                count_query = count_query.filter(request_logs::created_at.eq(created_at));
            }
            if let Some(request_id) = request_id.filter(|id| !id.trim().is_empty()) {
                query = query.filter(request_logs::request_id.eq(request_id));
                count_query = count_query.filter(request_logs::request_id.eq(request_id));
            }
            let total = count_query
                .count()
                .get_result::<i64>(&mut *c)
                .map_err(|e| e.to_string())? as u64;
            let items = query
                .order(request_logs::id.desc())
                .limit(i64::from(page_size))
                .offset(i64::from(page - 1) * i64::from(page_size))
                .select((
                    request_logs::id,
                    request_logs::request_id,
                    request_logs::created_at,
                    request_logs::method,
                    request_logs::path,
                    request_logs::model,
                    request_logs::upstream_model,
                    diesel::dsl::sql::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>>(
                        "length(CAST(inbound_json AS BLOB))",
                    ),
                    diesel::dsl::sql::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>>(
                        "length(CAST(upstream_json AS BLOB))",
                    ),
                    diesel::dsl::sql::<diesel::sql_types::Nullable<diesel::sql_types::BigInt>>(
                        "length(CAST(response_text AS BLOB))",
                    ),
                ))
                .load::<(
                    i64,
                    String,
                    i64,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    Option<i64>,
                    Option<i64>,
                    Option<i64>,
                )>(&mut *c)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(
                    |(
                        id,
                        request_id,
                        created_at,
                        method,
                        path,
                        model,
                        upstream_model,
                        inbound_bytes,
                        upstream_bytes,
                        response_bytes,
                    )| LogEntry {
                        id,
                        request_id,
                        created_at,
                        method,
                        path,
                        model,
                        upstream_model,
                        inbound_bytes,
                        upstream_bytes,
                        response_bytes,
                    },
                )
                .collect();
            Ok(Page::new(items, total, page, page_size))
        })
    }
    pub fn detail(&self, id: &str) -> Option<LogDetail> {
        crate::db::blocking(|| {
            let mut c = self.db.lock();
            request_logs::table
                .filter(request_logs::request_id.eq(id))
                .select(StoredLog::as_select())
                .first::<StoredLog>(&mut *c)
                .ok()
                .map(StoredLog::detail)
        })
    }
    pub fn cleanup_expired(&self) -> usize {
        crate::db::blocking(|| {
            let mut c = self.db.lock();
            match diesel::delete(
                request_logs::table
                    .filter(request_logs::created_at.lt(unix_secs() as i64 - RETENTION_SECS)),
            )
            .execute(&mut *c)
            {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!("request log cleanup: {e}");
                    0
                }
            }
        })
    }
    pub fn vacuum(&self) {
        crate::db::blocking(|| {
            if let Err(e) = self
                .db
                .lock()
                .batch_execute("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
            {
                tracing::warn!("request log vacuum: {e}");
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn store() -> RequestLogStore {
        RequestLogStore::new(Db::open(":memory:").unwrap())
    }
    #[test]
    fn insert_update_duplicate_and_list() {
        let s = store();
        s.insert_request("a", "POST", "/v1/x", &json!({"n":1,"model":"gpt-4o"}));
        assert!(s.detail("a").unwrap().upstream_model.is_none());
        s.update_upstream("a", &json!({"ok":true,"model":"mapped-model"}));
        s.update_response("a", "你好");
        s.insert_request("a", "POST", "/x", &json!({"n":2}));
        let d = s.detail("a").unwrap();
        assert_eq!(d.model.as_deref(), Some("gpt-4o"));
        assert_eq!(d.upstream_model.as_deref(), Some("mapped-model"));
        assert!(d.inbound_json.unwrap().contains("1"));
        assert!(d.upstream_json.unwrap().contains("true"));
        let rows = s.list((1, 10), None, None).unwrap();
        assert_eq!(rows.items[0].response_bytes, Some(6));
        assert_eq!(rows.items[0].model.as_deref(), Some("gpt-4o"));
        assert_eq!(
            rows.items[0].upstream_model.as_deref(),
            Some("mapped-model")
        );
    }
    #[test]
    fn upstream_model_follows_final_body_even_without_inbound_capture() {
        let s = store();
        s.insert_request_without_body("a", "POST", "/v1/responses");
        for body in [
            json!({"model":"first-model"}),
            json!({"model":"retry-model"}),
            json!({}),
            json!({"model":123}),
        ] {
            s.update_upstream("a", &body);
            let detail = s.detail("a").unwrap();
            assert!(detail.model.is_none());
            assert!(detail.inbound_json.is_none());
            assert_eq!(detail.upstream_model.as_deref(), body["model"].as_str());
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&detail.upstream_json.unwrap()).unwrap(),
                body
            );
        }
    }
    #[test]
    fn legacy_schema_migration_preserves_rows_and_can_run_repeatedly() {
        for has_model in [false, true] {
            let db = Db::open(":memory:").unwrap();
            let model_column = if has_model { "model TEXT," } else { "" };
            {
                let mut conn = db.lock();
                conn.batch_execute(&format!(
                    "CREATE TABLE request_logs (id INTEGER PRIMARY KEY AUTOINCREMENT, request_id TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL, method TEXT, path TEXT, {model_column} inbound_json TEXT, upstream_json TEXT, response_text TEXT);"
                )).unwrap();
                diesel::insert_into(request_logs::table)
                    .values((
                        request_logs::request_id.eq("legacy"),
                        request_logs::created_at.eq(unix_secs() as i64),
                        request_logs::upstream_json.eq(r#"{"model":"historical-model"}"#),
                    ))
                    .execute(&mut *conn)
                    .unwrap();
                if has_model {
                    diesel::update(request_logs::table)
                        .set(request_logs::model.eq("client-model"))
                        .execute(&mut *conn)
                        .unwrap();
                }
            }
            let s = RequestLogStore::new(db.clone());
            let legacy = s.detail("legacy").unwrap();
            assert_eq!(legacy.model.as_deref(), has_model.then_some("client-model"));
            assert!(legacy.upstream_model.is_none());
            assert_eq!(
                legacy.upstream_json.as_deref(),
                Some(r#"{"model":"historical-model"}"#)
            );
            assert!(
                s.list((1, 10), None, None).unwrap().items[0]
                    .upstream_model
                    .is_none()
            );
            s.update_upstream("legacy", &json!({"model":"mapped-model"}));
            let reopened = RequestLogStore::new(db);
            let detail = reopened.detail("legacy").unwrap();
            assert_eq!(detail.model.as_deref(), has_model.then_some("client-model"));
            assert_eq!(detail.upstream_model.as_deref(), Some("mapped-model"));
        }
    }
    #[test]
    fn pagination_and_cleanup() {
        let s = store();
        let now = unix_secs() as i64;
        for (id, t) in [("old", now - RETENTION_SECS - 1), ("new", now)] {
            let mut c = s.db.lock();
            diesel::insert_into(request_logs::table)
                .values((
                    request_logs::request_id.eq(id),
                    request_logs::created_at.eq(t),
                ))
                .execute(&mut *c)
                .unwrap();
        }
        assert_eq!(s.cleanup_expired(), 1);
        assert!(s.detail("old").is_none());
        assert!(
            matches!(s.list((1, 1), None, None).unwrap(), page if page.total == 1 && page.items.len() == 1)
        );
    }
}
