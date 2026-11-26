//! 请求日志存储:request_logs 表的写入、查询与过期清理。
use crate::{
    core::page::Page,
    log::wire::{LogDetail, LogEntry},
    storage::{db::Db, schema::request_logs},
    util::unix_secs,
};
use axum::http::HeaderMap;
use diesel::{
    connection::SimpleConnection,
    dsl::sql,
    prelude::*,
    sql_types::{Bool, Text},
};

use super::context::headers_to_text;

pub const RETENTION_SECS: i64 = 7 * 24 * 60 * 60;
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS request_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT, request_id TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL,
    method TEXT, path TEXT, model TEXT, upstream_model TEXT, effort TEXT,
    inbound_headers TEXT, inbound_json TEXT, upstream_headers TEXT, upstream_json TEXT,
    response_headers TEXT, response_text TEXT
); CREATE INDEX IF NOT EXISTS idx_request_logs_created ON request_logs(created_at);";

/// 客户端请求体里的 effort:chat `reasoning_effort` / messages `output_config.effort` / responses `reasoning.effort`。
fn request_effort(body: &serde_json::Value) -> Option<&str> {
    body.get("reasoning_effort")
        .or_else(|| body.pointer("/output_config/effort"))
        .or_else(|| body.pointer("/reasoning/effort"))
        .and_then(|e| e.as_str())
}

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
    effort: Option<String>,
    inbound_headers: Option<String>,
    inbound_json: Option<String>,
    upstream_headers: Option<String>,
    upstream_json: Option<String>,
    response_headers: Option<String>,
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
    effort: Option<&'a str>,
    inbound_headers: Option<&'a str>,
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
            effort: self.effort,
            inbound_headers: self.inbound_headers,
            inbound_json: self.inbound_json,
            upstream_headers: self.upstream_headers,
            upstream_json: self.upstream_json,
            response_headers: self.response_headers,
            response_text: self.response_text,
        }
    }
}
pub struct RequestLogStore {
    db: Db,
}
impl RequestLogStore {
    pub fn new(db: Db) -> Self {
        crate::storage::db::blocking(|| {
            let migration = db.lock().immediate_transaction(|conn| {
                conn.batch_execute(SCHEMA)?;
                for column in [
                    "model",
                    "upstream_model",
                    "effort",
                    "inbound_headers",
                    "upstream_headers",
                    "response_headers",
                ] {
                    let exists = diesel::select(
                        sql::<Bool>(
                            "EXISTS (SELECT 1 FROM pragma_table_info('request_logs') WHERE name = ",
                        )
                        .bind::<Text, _>(column)
                        .sql(")"),
                    )
                    .get_result::<bool>(conn)?;
                    if !exists {
                        conn.batch_execute(&format!(
                            "ALTER TABLE request_logs ADD COLUMN {column} TEXT"
                        ))?;
                    }
                }
                Ok::<_, diesel::result::Error>(())
            });
            if let Err(e) = migration {
                crate::trace_error!("log-store", "request log schema: {}", e);
            }
            let store = Self { db };
            if store.cleanup_expired() > 0 {
                store.vacuum();
            }
            store
        })
    }
    pub fn count(&self) -> Result<u64, String> {
        crate::storage::db::blocking(|| {
            let mut c = self.db.lock();
            request_logs::table
                .count()
                .get_result::<i64>(&mut *c)
                .map(|n| n as u64)
                .map_err(|e| e.to_string())
        })
    }
    pub fn insert_request(&self, id: &str, method: &str, path: &str, body: &serde_json::Value) {
        self.insert_inbound(id, method, path, None, Some(body));
    }
    pub fn insert_request_without_body(&self, id: &str, method: &str, path: &str) {
        self.insert_inbound(id, method, path, None, None);
    }
    pub(crate) fn insert_inbound(
        &self,
        id: &str,
        method: &str,
        path: &str,
        headers: Option<&HeaderMap>,
        body: Option<&serde_json::Value>,
    ) {
        crate::storage::db::blocking(|| {
            let headers = headers.map(headers_to_text);
            let json = body.and_then(|body| serde_json::to_string_pretty(body).ok());
            let mut c = self.db.lock();
            if let Err(e) = diesel::insert_into(request_logs::table)
                .values(NewLog {
                    request_id: id,
                    created_at: unix_secs() as i64,
                    method: Some(method),
                    path: Some(path),
                    model: body
                        .and_then(|body| body.get("model"))
                        .and_then(|m| m.as_str()),
                    effort: body.and_then(request_effort),
                    inbound_headers: headers.as_deref(),
                    inbound_json: json.as_deref(),
                })
                .on_conflict(request_logs::request_id)
                .do_nothing()
                .execute(&mut *c)
            {
                crate::trace_warn!("log-store", "request log insert ({}): {}", id, e);
            }
        })
    }
    pub fn update_upstream(&self, id: &str, body: &serde_json::Value) {
        self.write_upstream(id, body, None);
    }
    pub(crate) fn record_upstream_request(&self, id: &str, request: &reqwest::Request) {
        let body = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .unwrap_or_default();
        self.write_upstream(
            id,
            &super::context::body_to_value(body),
            Some(request.headers()),
        );
    }
    fn write_upstream(&self, id: &str, body: &serde_json::Value, headers: Option<&HeaderMap>) {
        crate::storage::db::blocking(|| {
            // 从最终发送的请求体提取,避免记录成映射前的客户端模型。
            let upstream_model = body.get("model").and_then(|model| model.as_str());
            let upstream_effort = body.get("reasoning_effort").and_then(|e| e.as_str());
            let body = serde_json::to_string_pretty(body).unwrap_or_default();
            let headers = headers.map(headers_to_text);
            let mut c = self.db.lock();
            // effort 列形如 "请求(上游)";重试会多次调用,所以只取 '(' 之前的请求侧再重拼。
            let stored: Option<String> = request_logs::table
                .filter(request_logs::request_id.eq(id))
                .select(request_logs::effort)
                .first(&mut *c)
                .ok()
                .flatten();
            let requested = stored
                .as_deref()
                .and_then(|e| e.split('(').next())
                .filter(|e| !e.is_empty() && *e != "-");
            let effort = (requested.is_some() || upstream_effort.is_some()).then(|| {
                format!(
                    "{}({})",
                    requested.unwrap_or("-"),
                    upstream_effort.unwrap_or("-")
                )
            });
            if let Err(e) =
                diesel::update(request_logs::table.filter(request_logs::request_id.eq(id)))
                    .set((
                        request_logs::upstream_json.eq(Some(body)),
                        request_logs::upstream_model.eq(upstream_model),
                        request_logs::effort.eq(effort),
                        // 新一轮尝试只覆盖请求侧;响应侧保留到新响应真正到达时才被
                        // 覆盖,这样重试失败时仍能看到上一轮的响应。
                        headers.map(|headers| request_logs::upstream_headers.eq(Some(headers))),
                    ))
                    .execute(&mut *c)
            {
                crate::trace_warn!("log-store", "request log upstream ({}): {}", id, e);
            }
        })
    }
    pub(crate) fn record_response_headers(&self, id: &str, headers: &HeaderMap) {
        crate::storage::db::blocking(|| {
            let headers = headers_to_text(headers);
            let mut conn = self.db.lock();
            if let Err(e) =
                diesel::update(request_logs::table.filter(request_logs::request_id.eq(id)))
                    .set(request_logs::response_headers.eq(Some(headers)))
                    .execute(&mut *conn)
            {
                crate::trace_warn!("log-store", "request log response headers ({}): {}", id, e);
            }
        })
    }
    pub fn update_response(&self, id: &str, body: &str) {
        crate::storage::db::blocking(|| {
            let mut c = self.db.lock();
            if let Err(e) =
                diesel::update(request_logs::table.filter(request_logs::request_id.eq(id)))
                    .set(request_logs::response_text.eq(Some(body)))
                    .execute(&mut *c)
            {
                crate::trace_warn!("log-store", "request log response ({}): {}", id, e);
            }
        })
    }
    pub fn list(
        &self,
        (page, page_size): (u32, u32),
        created_at: Option<i64>,
        request_id: Option<&str>,
    ) -> Result<Page<LogEntry>, String> {
        crate::storage::db::blocking(|| {
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
                    request_logs::effort,
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
                        effort,
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
                        effort,
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
        crate::storage::db::blocking(|| {
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
        crate::storage::db::blocking(|| {
            let mut c = self.db.lock();
            match diesel::delete(
                request_logs::table
                    .filter(request_logs::created_at.lt(unix_secs() as i64 - RETENTION_SECS)),
            )
            .execute(&mut *c)
            {
                Ok(n) => n,
                Err(e) => {
                    crate::trace_warn!("log-store", "request log cleanup: {}", e);
                    0
                }
            }
        })
    }
    pub fn vacuum(&self) {
        crate::storage::db::blocking(|| {
            if let Err(e) = self
                .db
                .lock()
                .batch_execute("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
            {
                crate::trace_warn!("log-store", "request log vacuum: {}", e);
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
    fn effort_combines_request_and_upstream_and_survives_retry() {
        let s = store();
        s.insert_request("a", "POST", "/v1/x", &json!({"reasoning":{"effort":"max"}}));
        assert_eq!(s.detail("a").unwrap().effort.as_deref(), Some("max"));
        for _ in 0..2 {
            s.update_upstream("a", &json!({"reasoning_effort":"xhigh"}));
            assert_eq!(s.detail("a").unwrap().effort.as_deref(), Some("max(xhigh)"));
        }
        s.insert_request_without_body("b", "POST", "/v1/x");
        s.update_upstream("b", &json!({"reasoning_effort":"high"}));
        assert_eq!(
            s.list((1, 10), None, None).unwrap().items[0]
                .effort
                .as_deref(),
            Some("-(high)")
        );
        s.update_upstream("b", &json!({}));
        assert!(s.detail("b").unwrap().effort.is_none());
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
    fn new_upstream_request_keeps_previous_response_until_a_new_one_arrives() {
        let s = store();
        s.insert_request_without_body("a", "POST", "/v1/responses");
        let client = reqwest::Client::new();
        let request = |attempt: &str| {
            client
                .post("http://127.0.0.1/")
                .header("x-test-attempt", attempt)
                .body(json!({"model": "mapped-model", "attempt": attempt}).to_string())
                .build()
                .unwrap()
        };
        s.record_upstream_request("a", &request("first"));
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "text/event-stream".parse().unwrap());
        s.record_response_headers("a", &headers);
        s.update_response("a", "upstream error");
        s.record_upstream_request("a", &request("retry"));

        let detail = s.detail("a").unwrap();
        assert_eq!(
            detail.upstream_headers.as_deref(),
            Some("x-test-attempt: retry")
        );
        assert_eq!(detail.upstream_model.as_deref(), Some("mapped-model"));
        assert!(detail.upstream_json.unwrap().contains("retry"));
        // 重试尚未收到响应:上一轮的响应仍保留,便于排查重试为何失败。
        assert_eq!(
            detail.response_headers.as_deref(),
            Some("content-type: text/event-stream")
        );
        assert_eq!(detail.response_text.as_deref(), Some("upstream error"));

        // 新响应到达后整体覆盖。
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        s.record_response_headers("a", &headers);
        s.update_response("a", "ok");
        let detail = s.detail("a").unwrap();
        assert_eq!(
            detail.response_headers.as_deref(),
            Some("content-type: application/json")
        );
        assert_eq!(detail.response_text.as_deref(), Some("ok"));
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
            assert!(legacy.inbound_headers.is_none());
            assert!(legacy.upstream_headers.is_none());
            assert!(legacy.response_headers.is_none());
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
