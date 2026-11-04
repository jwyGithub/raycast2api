//! src/log/store.rs
//! 请求日志存储:request_logs 表的写入、查询与过期清理。

use crate::db::Db;
use crate::log::wire::{LogDetail, LogEntry};
use crate::util::unix_secs;

/// 保留窗口:7 天(秒)。
pub const RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS request_logs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id    TEXT NOT NULL UNIQUE,
    created_at    INTEGER NOT NULL,
    method        TEXT,
    path          TEXT,
    inbound_json  TEXT,
    upstream_json TEXT,
    response_text TEXT
);
CREATE INDEX IF NOT EXISTS idx_request_logs_created ON request_logs(created_at);
";

pub struct RequestLogStore {
    pub(crate) db: Db,
}

impl RequestLogStore {
    /// 构造并建表(幂等);启动时清理过期日志,有删除时压缩数据库。
    pub fn new(db: Db) -> Self {
        if let Err(e) = db.lock().execute_batch(SCHEMA) {
            tracing::error!("request log schema: {e}");
        }
        let store = Self { db };
        if store.cleanup_expired() > 0 {
            store.vacuum();
        }
        store
    }

    /// 记录入站请求(INSERT OR IGNORE:同 id 重放不撞键、不覆盖)。
    pub fn insert_request(
        &self,
        request_id: &str,
        method: &str,
        path: &str,
        inbound: &serde_json::Value,
    ) {
        let inbound_json = serde_json::to_string_pretty(inbound).unwrap_or_default();
        let res = self.db.lock().execute(
            "INSERT OR IGNORE INTO request_logs
                (request_id, created_at, method, path, inbound_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![request_id, unix_secs() as i64, method, path, inbound_json,],
        );
        if let Err(e) = res {
            tracing::warn!("request log insert ({request_id}): {e}");
        }
    }

    /// 补写发给上游的请求体(行不存在时静默丢弃 —— 旁路,不影响请求)。
    pub fn update_upstream(&self, request_id: &str, upstream: &serde_json::Value) {
        let upstream_json = serde_json::to_string_pretty(upstream).unwrap_or_default();
        let res = self.db.lock().execute(
            "UPDATE request_logs SET upstream_json = ?2 WHERE request_id = ?1",
            rusqlite::params![request_id, upstream_json],
        );
        if let Err(e) = res {
            tracing::warn!("request log upstream ({request_id}): {e}");
        }
    }

    /// 补写上游 SSE 响应(NDJSON 原文)。
    pub fn update_response(&self, request_id: &str, response: &str) {
        let res = self.db.lock().execute(
            "UPDATE request_logs SET response_text = ?2 WHERE request_id = ?1",
            rusqlite::params![request_id, response],
        );
        if let Err(e) = res {
            tracing::warn!("request log response ({request_id}): {e}");
        }
    }

    /// 列表:id 降序(即时间降序),`before_id` 之前(更旧)的行,取前 `limit` 条。
    pub fn list(&self, limit: u32, before_id: Option<i64>) -> Vec<LogEntry> {
        let conn = self.db.lock();
        let sql = match before_id {
            Some(_) => "SELECT id, request_id, created_at, method, path,
                    octet_length(inbound_json), octet_length(upstream_json), octet_length(response_text)
                FROM request_logs WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
            None => "SELECT id, request_id, created_at, method, path,
                    octet_length(inbound_json), octet_length(upstream_json), octet_length(response_text)
                FROM request_logs ORDER BY id DESC LIMIT ?1",
        };
        let mut stmt = match conn.prepare(sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                tracing::warn!("request log list: {e}");
                return Vec::new();
            }
        };
        let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<LogEntry> {
            Ok(LogEntry {
                id: r.get(0)?,
                request_id: r.get(1)?,
                created_at: r.get(2)?,
                method: r.get(3)?,
                path: r.get(4)?,
                inbound_bytes: r.get(5)?,
                upstream_bytes: r.get(6)?,
                response_bytes: r.get(7)?,
            })
        };
        let rows = match before_id {
            Some(b) => stmt.query_map(rusqlite::params![b, limit], map_row),
            None => stmt.query_map(rusqlite::params![limit], map_row),
        };
        match rows {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(e) => {
                tracing::warn!("request log list: {e}");
                Vec::new()
            }
        }
    }

    /// 详情:三体全文。
    pub fn detail(&self, request_id: &str) -> Option<LogDetail> {
        let conn = self.db.lock();
        conn.query_row(
            "SELECT request_id, created_at, method, path,
                    inbound_json, upstream_json, response_text
             FROM request_logs WHERE request_id = ?1",
            [request_id],
            |r| {
                Ok(LogDetail {
                    request_id: r.get(0)?,
                    created_at: r.get(1)?,
                    method: r.get(2)?,
                    path: r.get(3)?,
                    inbound_json: r.get(4)?,
                    upstream_json: r.get(5)?,
                    response_text: r.get(6)?,
                })
            },
        )
        .ok()
    }

    /// 删除超过保留窗口的行,返回删除数量。
    pub fn cleanup_expired(&self) -> usize {
        let cutoff = unix_secs() as i64 - RETENTION_SECS;
        match self
            .db
            .lock()
            .execute("DELETE FROM request_logs WHERE created_at < ?1", [cutoff])
        {
            Ok(deleted) => deleted,
            Err(e) => {
                tracing::warn!("request log cleanup: {e}");
                0
            }
        }
    }

    /// 压缩 SQLite 文件并截断 WAL,回收已删除日志占用的磁盘空间。
    pub fn vacuum(&self) {
        if let Err(e) = self
            .db
            .lock()
            .execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
        {
            tracing::warn!("request log vacuum: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use serde_json::json;

    fn store() -> RequestLogStore {
        RequestLogStore::new(Db::open(":memory:").unwrap())
    }

    fn row(store: &RequestLogStore, request_id: &str, created_at: i64) {
        // 直接插一行指定时间的记录,绕过 insert_request 的 now(),测 cleanup/翻页。
        store
            .db
            .lock()
            .execute(
                "INSERT OR IGNORE INTO request_logs
                    (request_id, created_at, method, path, inbound_json)
                 VALUES (?1, ?2, 'POST', '/v1/x', '{}')",
                rusqlite::params![request_id, created_at],
            )
            .unwrap();
    }

    #[test]
    fn insert_then_updates_fill_columns_independently() {
        let s = store();
        s.insert_request("r1", "POST", "/v1/responses", &json!({"model": "m"}));
        s.update_upstream("r1", &json!({"provider": "openai"}));
        s.update_response("r1", "data: {\"text\":\"hi\"}");

        let d = s.detail("r1").unwrap();
        assert_eq!(d.method.as_deref(), Some("POST"));
        assert_eq!(d.path.as_deref(), Some("/v1/responses"));
        assert!(d.inbound_json.as_deref().unwrap().contains("\"model\""));
        assert!(d.upstream_json.as_deref().unwrap().contains("openai"));
        assert_eq!(d.response_text.as_deref(), Some("data: {\"text\":\"hi\"}"));
    }

    #[test]
    fn update_on_unknown_request_id_is_silently_dropped() {
        let s = store();
        // 旁路写入:未知 id 不报错、不产生行。
        s.update_upstream("ghost", &json!({}));
        s.update_response("ghost", "x");
        assert!(s.detail("ghost").is_none());
        assert!(s.list(50, None).is_empty());
    }

    #[test]
    fn duplicate_insert_is_ignored() {
        let s = store();
        s.insert_request("r1", "POST", "/a", &json!({"v": 1}));
        s.update_upstream("r1", &json!({"u": 1}));
        // 同 request_id 重复 insert:不报错,且不覆盖已补的列。
        s.insert_request("r1", "POST", "/a", &json!({"v": 2}));
        let d = s.detail("r1").unwrap();
        assert!(d.inbound_json.as_deref().unwrap().contains("\"v\": 1"));
        assert!(d.upstream_json.is_some());
    }

    #[test]
    fn list_is_descending_and_paginates_by_before_id() {
        let s = store();
        row(&s, "a", 100);
        row(&s, "b", 200);
        row(&s, "c", 300);

        let first = s.list(2, None);
        let page1: Vec<&str> = first.iter().map(|e| e.request_id.as_str()).collect();
        assert_eq!(page1, vec!["c", "b"]);
        let before_b = first[1].id;
        let second = s.list(2, Some(before_b));
        let page2: Vec<&str> = second.iter().map(|e| e.request_id.as_str()).collect();
        assert_eq!(page2, vec!["a"]);
    }

    #[test]
    fn entry_sizes_reflect_column_lengths() {
        let s = store();
        s.insert_request("r1", "POST", "/a", &json!({"x": 1}));
        s.update_response("r1", "hello");
        let e = &s.list(50, None)[0];
        let inbound_len = s.detail("r1").unwrap().inbound_json.unwrap().len() as i64;
        assert_eq!(e.inbound_bytes, Some(inbound_len));
        assert_eq!(e.response_bytes, Some(5));
        assert_eq!(e.upstream_bytes, None);
    }

    #[test]
    fn entry_sizes_count_bytes_not_characters() {
        let s = store();
        s.insert_request("r1", "POST", "/a", &json!({"x": 1}));
        // "你好" is 6 bytes in UTF-8 but only 2 characters.
        s.update_response("r1", "你好");
        let e = &s.list(50, None)[0];
        assert_eq!(e.response_bytes, Some(6));
    }

    #[test]
    fn cleanup_removes_expired_rows_only() {
        let s = store();
        let now = crate::util::unix_secs() as i64;
        row(&s, "old", now - RETENTION_SECS - 1);
        row(&s, "boundary", now - RETENTION_SECS);
        row(&s, "fresh", now);
        s.cleanup_expired();
        assert!(s.detail("old").is_none());
        assert!(s.detail("boundary").is_some());
        assert!(s.detail("fresh").is_some());
    }

    #[test]
    fn cleanup_compacts_database_after_deleting_rows() {
        let s = store();
        row(
            &s,
            "large",
            crate::util::unix_secs() as i64 - RETENTION_SECS - 1,
        );
        s.db.lock()
            .execute(
                "UPDATE request_logs SET inbound_json = ?1 WHERE request_id = 'large'",
                ["x".repeat(1024 * 1024)],
            )
            .unwrap();
        let pages_before: i64 =
            s.db.lock()
                .query_row("PRAGMA page_count", [], |r| r.get(0))
                .unwrap();

        assert_eq!(s.cleanup_expired(), 1);
        s.vacuum();

        let pages_after: i64 =
            s.db.lock()
                .query_row("PRAGMA page_count", [], |r| r.get(0))
                .unwrap();
        assert!(pages_after < pages_before);
        assert!(s.detail("large").is_none());
    }
}
