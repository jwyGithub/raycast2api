# 请求日志入库(request_logs)实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 DEBUG_DUMP_DIR 文件落盘的三件套(入站体/上游请求体/上游 SSE 响应)迁入 SQLite,按时间保留 3 天自动清理,并提供 HTTP 查询接口。

**Architecture:** 新 `src/log/` 域模块(照 account 模式建在共享 `db` 上):`request_logs` 表一行一请求、三列渐进补写;三个既有采集点(middleware / client / stream)从文件写切换为写库;`GET /v1/logs` 列表 + `GET /v1/logs/{request_id}` 详情,挂 API key 层内。

**Tech Stack:** Rust 2024 / rusqlite(已有)/ axum 0.8 / 既有分层(`server → log → db`)。

**Spec:** `docs/superpowers/specs/2026-09-14-request-logs-sqlite-design.md`(语义以它为准)

## Global Constraints

- 采集默认开启,无环境变量门控;体积不截断、无总量上限
- 保留窗口 3 天:`created_at < now - 259200` 秒即删;清理时机 = 启动(`RequestLogStore::new`)+ 每次 `insert_request` 顺带
- 日志写入是旁路:失败只 `warn!`,绝不影响请求(方法返回 `()`,不返回 Result)
- `request_id` 复用 `debug::REQUEST_ID` task-local ULID,唯一键
- 列表接口 `limit` 默认 50、上限 200、最小 1(超出范围 → 400);`before_id` 翻页;列表不含 body
- 时间戳一律 epoch 秒整数(`unix_secs()`,i64)
- 每任务结束 `cargo test` 全绿 + `cargo clippy --all-targets` 零警告后才提交
- 提交信息结尾带 `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

---

### Task 1: log 域模块(wire 类型 + store)

**Files:**
- Create: `src/log/mod.rs`
- Create: `src/log/wire.rs`
- Create: `src/log/store.rs`
- Modify: `src/lib.rs`(注册模块)

**Interfaces:**
- Consumes: `crate::db::Db`(`Db::open` / `Db::lock`)、`crate::util::unix_secs()`
- Produces:
  - `LogEntry`(Serialize;id: i64、request_id、created_at: i64、method/path: Option<String>、inbound_bytes/upstream_bytes/response_bytes: Option<i64>)
  - `LogDetail`(Serialize;request_id、created_at: i64、method/path、inbound_json/upstream_json/response_text: Option<String>)
  - `RequestLogStore::new(db: Db) -> Self`(建表 + 启动清理)
  - `insert_request(&self, request_id: &str, method: &str, path: &str, inbound: &serde_json::Value)`(INSERT OR IGNORE + 顺带 cleanup)
  - `update_upstream(&self, request_id: &str, upstream: &serde_json::Value)`
  - `update_response(&self, request_id: &str, response: &str)`
  - `list(&self, limit: u32, before_id: Option<i64>) -> Vec<LogEntry>`(id 降序)
  - `detail(&self, request_id: &str) -> Option<LogDetail>`
  - `pub const RETENTION_SECS: i64 = 259200`

- [ ] **Step 1: 模块骨架与类型**(`src/log/mod.rs`)

```rust
//! 请求日志域:推理请求的入站体、上游请求体与上游响应,存入 SQLite,
//! 按时间保留 3 天自动清理。建在 [`crate::db`] 之上;写入是旁路,
//! 任何失败只记日志,不影响请求处理。

pub mod store;
pub mod wire;
```

```rust
// src/log/wire.rs
//! 请求日志的对外 JSON 形状:列表项(不含 body)与详情(三体全文)。

use serde::Serialize;

/// `GET /v1/logs` 的一条列表项 —— 不含 body,只有元信息与各体大小。
#[derive(Debug, Serialize)]
pub struct LogEntry {
    pub id: i64,
    pub request_id: String,
    pub created_at: i64,
    pub method: Option<String>,
    pub path: Option<String>,
    pub inbound_bytes: Option<i64>,
    pub upstream_bytes: Option<i64>,
    pub response_bytes: Option<i64>,
}

/// `GET /v1/logs/{request_id}` 的详情 —— 三体全文。
#[derive(Debug, Serialize)]
pub struct LogDetail {
    pub request_id: String,
    pub created_at: i64,
    pub method: Option<String>,
    pub path: Option<String>,
    pub inbound_json: Option<String>,
    pub upstream_json: Option<String>,
    pub response_text: Option<String>,
}
```

- [ ] **Step 2: 写 store 失败测试**(先建 `src/log/store.rs` 只含 `use` 与空 `pub struct RequestLogStore;` + 测试,跑 `cargo test log` 确认编译失败)

```rust
// src/log/store.rs 末尾的测试模块
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

        let page1: Vec<&str> = s.list(2, None).iter().map(|e| e.request_id.as_str()).collect();
        assert_eq!(page1, vec!["c", "b"]);
        let before_b = s.list(2, None)[1].id;
        let page2: Vec<&str> =
            s.list(2, Some(before_b)).iter().map(|e| e.request_id.as_str()).collect();
        assert_eq!(page2, vec!["a"]);
    }

    #[test]
    fn entry_sizes_reflect_column_lengths() {
        let s = store();
        s.insert_request("r1", "POST", "/a", &json!({"x": 1}));
        s.update_response("r1", "hello");
        let e = &s.list(50, None)[0];
        assert_eq!(e.inbound_bytes, Some(7));  // pretty-printed "{\n  \"x\": 1\n}" 不对——见下
    }
}
```

**注意 `entry_sizes_reflect_column_lengths` 的断言值**:inbound 存的是 pretty JSON(`serde_json::to_string_pretty`),`json!({"x":1})` 的 pretty 形式是 `{\n  "x": 1\n}` 共 10 字节。**正确的断言是** `assert_eq!(e.inbound_bytes, Some(10));` 与 `assert_eq!(e.response_bytes, Some(5));`,并且为了不依赖 pretty 细节,写成:

```rust
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
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test log`
Expected: 编译失败(`RequestLogStore` 方法未定义)

- [ ] **Step 4: 实现 store**(tests 模块之前)

```rust
//! 请求日志存储:request_logs 表的写入、查询与过期清理。

use crate::db::Db;
use crate::log::wire::{LogDetail, LogEntry};
use crate::util::unix_secs;

/// 保留窗口:3 天(秒)。
pub const RETENTION_SECS: i64 = 3 * 24 * 60 * 60;

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
    /// 构造并建表(幂等);顺带做一次过期清理(即启动清理)。
    pub fn new(db: Db) -> Self {
        if let Err(e) = db.lock().execute_batch(SCHEMA) {
            tracing::error!("request log schema: {e}");
        }
        let store = Self { db };
        store.cleanup();
        store
    }

    /// 记录入站请求(INSERT OR IGNORE:同 id 重放不撞键、不覆盖)。
    /// 顺带清理过期行 —— created_at 有索引,无过期行时近零成本。
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
            rusqlite::params![
                request_id,
                unix_secs() as i64,
                method,
                path,
                inbound_json,
            ],
        );
        if let Err(e) = res {
            tracing::warn!("request log insert ({request_id}): {e}");
        }
        self.cleanup();
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
                    length(inbound_json), length(upstream_json), length(response_text)
                FROM request_logs WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
            None => "SELECT id, request_id, created_at, method, path,
                    length(inbound_json), length(upstream_json), length(response_text)
                FROM request_logs ORDER BY id DESC LIMIT ?2",
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
            Some(b) => stmt
                .query_map(rusqlite::params![b, limit], map_row)
                .map_err(|e| {
                    tracing::warn!("request log list: {e}");
                }),
            None => stmt
                .query_map(rusqlite::params![limit], map_row)
                .map_err(|e| {
                    tracing::warn!("request log list: {e}");
                }),
        };
        rows.map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
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

    /// 删除超过保留窗口的行。
    fn cleanup(&self) {
        let cutoff = unix_secs() as i64 - RETENTION_SECS;
        if let Err(e) = self.db.lock().execute(
            "DELETE FROM request_logs WHERE created_at < ?1",
            [cutoff],
        ) {
            tracing::warn!("request log cleanup: {e}");
        }
    }
}
```

**注意**:`list` 里 `query_map(...).map_err(|e| { warn; })` 返回 `Result<MappedRows, ()>` 的写法编译不过(`map_err` 到 `()` 后仍是 Result),请写成直接 match:

```rust
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
```

(即 `list` 的返回段用这个 match 替换上面代码中 `let rows = ...; rows.map(...)` 两段。)

- [ ] **Step 5: 注册模块** —— `src/lib.rs` 模块列表按字母序加 `pub mod log;`(在 `error` 与 `protocol` 之间),层注释加:

```rust
//! * [`log`]       — request-log storage (inbound/upstream/response, 3-day retention).
```

- [ ] **Step 6: 跑测试确认通过**

Run: `cargo test log`
Expected: 5 passed(insert/updates、unknown-id、duplicate、list 翻页、sizes)

- [ ] **Step 7: 清理一个测试盲点** —— `cleanup` 尚无直接测试(测试辅助 `row()` 已能插旧行)。在 tests 模块追加:

```rust
    #[test]
    fn cleanup_removes_expired_rows_only() {
        let s = store();
        let now = crate::util::unix_secs() as i64;
        row(&s, "old", now - RETENTION_SECS - 1);
        row(&s, "fresh", now);
        // insert_request 顺带触发 cleanup。
        s.insert_request("trigger", "POST", "/x", &serde_json::json!({}));
        assert!(s.detail("old").is_none());
        assert!(s.detail("fresh").is_some());
        assert!(s.detail("trigger").is_some());
    }
```

Run: `cargo test log`
Expected: 6 passed

- [ ] **Step 8: clippy + 提交**

```bash
cargo clippy --all-targets && git add src/log src/lib.rs && git commit -m "Add request-log store with three-column writes and 3-day cleanup

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: 采集点切换 + 装配 + 移除文件 dump

**Files:**
- Modify: `src/server/middleware.rs`(debug_capture 写库,恢复 State 提取)
- Modify: `src/server/mod.rs`(debug_capture 改 from_fn_with_state;层序不变:capture 先加=内层,auth 后加=外层)
- Modify: `src/raycast/client.rs`(加 `log` 字段;send_chat 的文件 dump 改 update_upstream)
- Modify: `src/raycast/stream.rs`(canonical_events 加 `log` 参数,Finish 时 update_response)
- Modify: `src/server/pipeline.rs`(传 `state.log.clone()`)
- Modify: `src/server/state.rs`(AppState 加 `log` 字段)
- Modify: `src/main.rs`(构造 RequestLogStore,装配)
- Modify: `src/debug.rs`(删 dump/dump_enabled/dump_dir,更新文档)
- Modify: `tests/accounts_contract.rs`(RaycastClient::new / AppState::new 调用点加 log 参数)

**Interfaces:**
- Consumes: `RequestLogStore`(Task 1)、`debug::{REQUEST_ID, current_request_id, body_to_value}`
- Produces:
  - `RaycastClient::new(http: Client, settings: Arc<Settings>, accounts: Arc<AccountStore>, log: Arc<RequestLogStore>) -> Self`
  - `AppState::new(settings, client, accounts, log: Arc<RequestLogStore>) -> Self`;`pub log: Arc<RequestLogStore>`
  - `canonical_events(response: reqwest::Response, log: Arc<RequestLogStore>) -> impl Stream<Item = CanonicalEvent>`(pipeline 唯一调用方)
  - `debug.rs` 只剩 `REQUEST_ID`、`current_request_id`、`body_to_value`

- [ ] **Step 1: middleware.rs** —— `debug_capture` 恢复 State 并写库。导入区把 `use axum::extract::{Request, State};` 与 `use super::state::SharedState;` 恢复(State 在 Task 前期被移除过,以当前文件为准补齐),函数整体替换为:

```rust
/// Buffer the inbound request body so it can be recorded in the request-log
/// store, then run the rest of the stack inside a [`REQUEST_ID`] scope so the
/// upstream and response writes correlate with it.
///
/// The body has to be buffered to be observed, so the request is reassembled
/// from the captured bytes before it continues — the downstream `Json`
/// extractor sees exactly what the client sent. Covers all authenticated
/// routes; log writes are fire-and-forget and never fail the request.
pub async fn debug_capture(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Result<Response, GatewayError> {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_CAPTURE_BYTES)
        .await
        .map_err(|_| GatewayError::BadRequest("request body too large or unreadable".into()))?;

    let id = util::new_ulid();
    state.log.insert_request(
        &id,
        parts.method.as_str(),
        parts.uri.path(),
        &debug::body_to_value(&bytes),
    );

    // Rebuild the request with the buffered body and run the handler within the
    // correlation-id scope; the upstream send happens inside this await.
    let request = Request::from_parts(parts, Body::from(bytes));
    Ok(REQUEST_ID.scope(id, next.run(request)).await)
}
```

- [ ] **Step 2: server/mod.rs** —— debug_capture 需要 state,把

```rust
        .layer(axum::middleware::from_fn(middleware::debug_capture))
```

改为

```rust
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::debug_capture,
        ))
```

**层序不变**:capture 先加(内层,认证后执行),auth 后加(外层)。上面这条仍在 `.layer(...require_api_key...)` 之前。

- [ ] **Step 3: client.rs** —— 结构体加字段与构造参数(照 accounts 字段先例),`send_chat` 中的文件 dump 块替换:

```rust
        // Record the outgoing upstream body in the request-log store,
        // correlated to the inbound request via the task-local id set by
        // `debug_capture`. Fire-and-forget.
        self.log
            .update_upstream(&debug::current_request_id(), &debug::body_to_value(body.as_bytes()));
```

imports 加 `use crate::log::store::RequestLogStore;`,`new` 完整签名:

```rust
    pub fn new(
        http: Client,
        settings: Arc<Settings>,
        accounts: Arc<AccountStore>,
        log: Arc<RequestLogStore>,
    ) -> Self {
        Self {
            http,
            settings,
            accounts,
            log,
        }
    }
```

(同时删除 send_chat 里原有的 `let body_value = ...; if let Ok(text) = ... { debug::dump(...) }` 文件块。)

- [ ] **Step 4: stream.rs** —— `canonical_events` 加参数,文件 dump 改写库,移除 `dump_enabled` 分支:

```rust
use async_stream::stream;
use futures_util::{Stream, StreamExt};

use crate::core::event::CanonicalEvent;
use crate::debug;
use crate::log::store::RequestLogStore;

use super::parser::parse_events;
use super::sse::SseDecoder;

use std::sync::Arc;

/// Decode a Raycast chat SSE response into a stream of [`CanonicalEvent`]s.
pub fn canonical_events(
    response: reqwest::Response,
    log: Arc<RequestLogStore>,
) -> impl Stream<Item = CanonicalEvent> {
    // Grab the correlation id *here*: the task-local REQUEST_ID scope is only
    // valid inside the handler, while the stream below is polled outside it.
    // The stream! block is an async move, so both are moved into it.
    let id = debug::current_request_id();

    stream! {
        let mut payloads: Vec<String> = Vec::new();
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::new();

        while let Some(chunk) = bytes.next().await {
            let Ok(chunk) = chunk else { break };
            for payload in decoder.push_bytes(&chunk) {
                payloads.push(payload.clone());
                // ponytail: 打印原始 SSE 数据用于调试
                tracing::debug!("upstream SSE payload: {}", payload);

                for event in parse_events(&payload) {
                    // The outbound adapters stop polling the stream at `Finish`,
                    // so this generator's tail code never runs on the streaming
                    // path — record as soon as the terminating payload is seen.
                    // Anything after it is `[DONE]` / notifications.
                    if matches!(event, CanonicalEvent::Finish { .. }) {
                        log.update_response(&id, &payloads.join("\n"));
                    }
                    // 打印解析后的规范事件
                    match &event {
                        CanonicalEvent::TextDelta(text) => {
                            tracing::info!("→ text: {}", text);
                        }
                        CanonicalEvent::ReasoningDelta(text) => {
                            tracing::info!("→ reasoning: {}", text);
                        }
                        CanonicalEvent::ThoughtSignature(sig) => {
                            tracing::info!("→ thought_signature: {}", sig);
                        }
                        CanonicalEvent::ToolCall(call) => {
                            tracing::info!("→ tool_call: {} (id: {})", call.name, call.id);
                        }
                        CanonicalEvent::Finish { reason, usage } => {
                            tracing::info!("→ finish: reason={:?}, tokens={{input: {}, output: {}, cached: {}}}",
                                reason, usage.input_tokens, usage.output_tokens, usage.cached_tokens);
                        }
                    }
                    yield event;
                }
            }
        }

        // One raw `data:` payload per line, exactly as the upstream sent it
        // (non-streaming aggregation path polls the stream to completion).
        log.update_response(&id, &payloads.join("\n"));
    }
}
```

- [ ] **Step 5: pipeline.rs** —— `let events = canonical_events(response);` 改为 `let events = canonical_events(response, state.log.clone());`

- [ ] **Step 6: state.rs** —— `AppState` 加 `pub log: Arc<RequestLogStore>;` 字段(照 `accounts` 先例,含 doc 注释"请求日志存储"),`new` 加第四参数 `log: Arc<RequestLogStore>`,imports 加 `use crate::log::store::RequestLogStore;`

- [ ] **Step 7: main.rs** —— accounts 构造之后追加:

```rust
    let request_log = Arc::new(raycast2api::log::store::RequestLogStore::new(db));
```

**注意**:当前 `main.rs` 里 `Db::open` 后直接 `AccountStore::new(db)` 把 `db` move 走了 —— `Db` 是 `Clone`(内部 Arc),改为 `AccountStore::new(db.clone())`,再建 `RequestLogStore::new(db)`。`RaycastClient::new(http, settings.clone(), accounts.clone(), request_log.clone())`;`AppState::new(settings.clone(), client.clone(), accounts.clone(), request_log)`。

- [ ] **Step 8: debug.rs** —— 删除 `dump`、`dump_enabled`、`dump_dir` 三个函数,模块文档替换为:

```rust
//! Request-correlation support for the request-log store.
//!
//! [`REQUEST_ID`] is a task-local correlation id set by the `debug_capture`
//! middleware; the upstream client and the SSE bridge use it to attribute
//! their log writes to the originating inbound request — without threading an
//! id through every handler and pipeline signature.
```

(`REQUEST_ID`、`current_request_id`、`body_to_value` 保留原样。)

- [ ] **Step 9: tests/accounts_contract.rs** —— `app()` 里构造处同步新签名:

```rust
    let client = raycast2api::raycast::client::RaycastClient::new(
        reqwest::Client::new(),
        settings.clone(),
        store.clone(),
        log_store.clone(),
    );
    let state = Arc::new(raycast2api::server::state::AppState::new(
        settings,
        client,
        store.clone(),
        log_store,
    ));
```

其中 `let log_store = Arc::new(raycast2api::log::store::RequestLogStore::new(Db::open(":memory:").unwrap()));`。

- [ ] **Step 10: 验证 + 提交**

Run: `cargo test`
Expected: 全绿(既有套件 + accounts_contract 3)

```bash
cargo clippy --all-targets && git add -A ':!debug-dump' && git commit -m "Switch request capture to SQLite store; drop DEBUG_DUMP_DIR files

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: 查询路由 + 契约测试 + README

**Files:**
- Create: `src/server/handlers/logs.rs`
- Modify: `src/server/handlers/mod.rs`(加 `pub mod logs;`,字母序)
- Modify: `src/server/mod.rs`(两条路由)
- Create: `tests/logs_contract.rs`
- Modify: `README.md`(DEBUG_DUMP_DIR 段替换为请求日志接口)

**Interfaces:**
- Consumes: `RequestLogStore::{list, detail}`(Task 1)、`AppState.log`
- Produces(路由,`require_api_key` 层内):
  - `GET /v1/logs?limit=50&before_id=<id>` → `Json<Vec<LogEntry>>`;limit 范围 1..=200,越界 400;缺省 50
  - `GET /v1/logs/{request_id}` → `Json<LogDetail>`;不存在 404

- [ ] **Step 1: handler**

```rust
//! `/v1/logs` — 请求日志查询(列表 + 详情)。

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;

use crate::error::GatewayError;
use crate::log::wire::{LogDetail, LogEntry};
use crate::server::state::SharedState;

/// 列表查询参数。
#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub limit: Option<u32>,
    /// 翻页:取 id 小于该值的更旧记录。
    pub before_id: Option<i64>,
}

/// `GET /v1/logs` — 列表(降序,不含 body)。
pub async fn list(
    State(state): State<SharedState>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<LogEntry>>, GatewayError> {
    let limit = params.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return Err(GatewayError::BadRequest(format!(
            "limit must be in 1..=200, got {limit}"
        )));
    }
    Ok(Json(state.log.list(limit, params.before_id)))
}

/// `GET /v1/logs/{request_id}` — 详情(三体全文);不存在 404。
pub async fn detail(
    State(state): State<SharedState>,
    Path(request_id): Path<String>,
) -> Result<Json<LogDetail>, GatewayError> {
    state
        .log
        .detail(&request_id)
        .map(Json)
        .ok_or_else(|| GatewayError::NotFound(format!("no request log {request_id}")))
}
```

- [ ] **Step 2: 路由** —— `src/server/mod.rs` 的 `api` 链上(`/v1/accounts` 段之后)加:

```rust
        .route("/v1/logs", get(handlers::logs::list))
        .route("/v1/logs/{request_id}", get(handlers::logs::detail))
```

`src/server/handlers/mod.rs` 按字母序加 `pub mod logs;`(在 `health` 与 `messages` 之间)。

- [ ] **Step 3: 契约测试**(`tests/logs_contract.rs`;app 构造照 accounts_contract,共用一个 RequestLogStore)

```rust
//! `/v1/logs` 查询接口的契约测试(内存库)。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use raycast2api::account::store::AccountStore;
use raycast2api::config::Settings;
use raycast2api::db::Db;
use raycast2api::log::store::RequestLogStore;
use raycast2api::server::build_router;
use raycast2api::server::state::AppState;
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> (axum::Router, Arc<RequestLogStore>) {
    let db = Db::open(":memory:").unwrap();
    let store = Arc::new(AccountStore::new(db.clone()));
    let log_store = Arc::new(RequestLogStore::new(db));
    let settings = Arc::new(Settings {
        raycast_device_id: "device".into(),
        raycast_signature_secret: None,
        api_key: Some("test-key".into()),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        bind_addr_dual: None,
        show_advanced: true,
        include_deprecated: true,
        model_overrides: Default::default(),
        accounts_db: ":memory:".into(),
    });
    let client = raycast2api::raycast::client::RaycastClient::new(
        reqwest::Client::new(),
        settings.clone(),
        store.clone(),
        log_store.clone(),
    );
    let state = Arc::new(AppState::new(settings, client, store, log_store.clone()));
    (build_router(state), log_store)
}

async fn get(app: &axum::Router, uri: &str, key: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(Method::GET).uri(uri);
    if let Some(key) = key {
        builder = builder.header("authorization", format!("Bearer {key}"));
    }
    let request = builder.body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

#[tokio::test]
async fn list_and_detail_round_trip() {
    let (app, log) = app();
    log.insert_request("req_1", "POST", "/v1/responses", &json!({"model": "m"}));
    log.update_upstream("req_1", &json!({"provider": "openai"}));
    log.update_response("req_1", "data: {\"text\":\"hi\"}");

    // 列表:不含任何 body 字段,含大小。
    let (status, body) = get(&app, "/v1/logs", Some("test-key")).await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["request_id"], "req_1");
    assert_eq!(arr[0]["method"], "POST");
    assert!(arr[0].get("inbound_json").is_none());
    assert!(arr[0].get("response_text").is_none());
    assert!(arr[0]["response_bytes"].as_i64().unwrap() > 0);

    // 详情:三体齐全。
    let (status, body) = get(&app, "/v1/logs/req_1", Some("test-key")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["request_id"], "req_1");
    assert!(body["inbound_json"].as_str().unwrap().contains("\"model\""));
    assert!(body["upstream_json"].as_str().unwrap().contains("openai"));
    assert_eq!(body["response_text"], "data: {\"text\":\"hi\"}");
}

#[tokio::test]
async fn detail_unknown_id_is_404() {
    let (app, _log) = app();
    let (status, _) = get(&app, "/v1/logs/nope", Some("test-key")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn limit_out_of_range_is_400() {
    let (app, _log) = app();
    let (status, _) = get(&app, "/v1/logs?limit=0", Some("test-key")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(&app, "/v1/logs?limit=201", Some("test-key")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn missing_api_key_is_unauthorized() {
    let (app, _log) = app();
    let (status, _) = get(&app, "/v1/logs", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
```

- [ ] **Step 4: 跑测试**

Run: `cargo test --test logs_contract`
Expected: 4 passed

- [ ] **Step 5: README** —— 找到请求落盘(DEBUG_DUMP_DIR)相关段落,替换为:

```markdown
- **请求日志**：每个推理请求的入站体、发给上游的请求体与上游 SSE 响应自动存入 SQLite（`store/store.db`），按时间保留 3 天自动清理。`GET /v1/logs` 查看列表（`limit` 默认 50、上限 200，`before_id` 翻页），`GET /v1/logs/{request_id}` 查看三体全文。
```

并在接口表中追加:

```markdown
| `GET /v1/logs` | 请求日志列表（不含请求体） | — |
| `GET /v1/logs/{request_id}` | 请求日志详情（入站/上游/响应全文） | — |
```

- [ ] **Step 6: 全量验证 + 提交**

Run: `cargo test && cargo clippy --all-targets && cargo build`
Expected: 全绿、零警告、构建成功

```bash
git add -A ':!debug-dump' && git commit -m "Add /v1/logs query routes and document request logging

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## 自检记录

- **Spec 覆盖**:表结构与三列渐进补写(T1)、三个采集点+注入+移除文件 dump(T2)、查询接口+limit 语义+404(T3)、3 天清理(new 启动 + insert 顺带,T1)、旁路容错(T1 warn + T2 fire-and-forget)—— 全覆盖。
- **占位符**:无 TBD;Task 1 Step 4 对 `list` 返回段给出了明确的 match 替换写法(计划代码中标注的编译问题已内联修正说明)。
- **类型一致性**:`RequestLogStore::{insert_request, update_upstream, update_response, list, detail}`、`RaycastClient::new(http, settings, accounts, log)`、`AppState::new(settings, client, accounts, log)`、`canonical_events(response, log)` 在 T1/T2/T3 间一致;`LogEntry`/`LogDetail` 字段与 store 映射列一一对应。
