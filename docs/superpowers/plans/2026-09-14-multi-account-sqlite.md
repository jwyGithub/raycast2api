# 多账号管理(SQLite)实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 网关支持多 Raycast 账号:SQLite 存储、启动导入 account.json、请求轮询、CRUD 接口、额度定时刷新、401/额度耗尽自动摘除。

**Architecture:** 新增共享 `db` 模块(bundled rusqlite,`Arc<Mutex<Connection>>`)与 `account` 域模块(store/import/refresh/wire 四文件),`raycast::client` 注入 `Arc<AccountStore>` 取代单 bearer token,HTTP 层加 4 个账号路由,`main` 装配启动导入与 6 小时刷新循环。

**Tech Stack:** Rust 2024 / axum 0.8 / rusqlite 0.32(bundled)/ tokio / 既有分层(`server → account → db`)。

**Spec:** `docs/superpowers/specs/2026-09-14-multi-account-sqlite-design.md`(已批准,本计划的一切语义以它为准)

## Global Constraints

- 唯一标识为 `user.email`;upsert 三态:插入 / token 变化则更新(重置 available=1)/ token 相同跳过
- 推理只用 SQLite 账号;`config.json` 的 `raycast_bearer_token` 字段移除(serde 宽容,旧文件无需改)
- 不做 token 刷新;401 与额度耗尽摘除,有余额恢复
- 额度刷新:启动立即一遍 + 每 6 小时;`POST /v1/accounts/refresh` 手动触发(202)
- 时间戳一律 `unix_secs().to_string()`(epoch 秒,TEXT),不引入时间库
- 每个任务结束 `cargo test` 全绿 + `cargo clippy --all-targets` 零警告后才提交
- 提交信息结尾带 `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`

---

### Task 1: db 模块(共享 SQLite 基础设施)

**Files:**
- Modify: `Cargo.toml`(加 rusqlite)
- Create: `src/db.rs`
- Modify: `src/lib.rs`(注册模块)

**Interfaces:**
- Consumes: 无(最底层)
- Produces: `pub struct Db`(`Arc<Mutex<rusqlite::Connection>>`);`Db::open(path: &str) -> Result<Db, String>`;`Db::lock(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection>`。后续所有业务 store 只经这两个成员使用它。

- [ ] **Step 1: 加依赖**

```toml
# Cargo.toml [dependencies] 追加
# Shared SQLite storage for the account store (and future modules).
rusqlite = { version = "0.32", features = ["bundled"] }
```

- [ ] **Step 2: 写失败测试**(创建 `src/db.rs`,先只含测试会引用的骨架不行 —— 直接写完整文件,但先跑测试确认编译期失败即可)

```rust
//! src/db.rs
//! 共享 SQLite 基础设施:连接管理,不含任何业务表知识。
//!
//! 各业务模块(如 `account`)的 store 持有一个 [`Db`],自己执行各自的
//! `CREATE TABLE IF NOT EXISTS`。新增使用方不需要改动本文件。

use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::Connection;

/// 一个共享的 SQLite 连接(引用计数 + 互斥)。
///
/// 网关流量下 SQLite 操作是微秒级,一把互斥锁足够;若未来需要异步
/// 驱动,只替换本文件,业务 store 的 `&Db` 签名不变。
#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    /// 打开(或创建)数据库文件;`:memory:` 用于测试。
    /// WAL 与 busy timeout 让多线程/多进程读写更稳。
    pub fn open(path: &str) -> Result<Self, String> {
        let conn = Connection::open(path)
            .map_err(|e| format!("failed to open sqlite db '{path}': {e}"))?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
            .map_err(|e| format!("failed to configure sqlite db '{path}': {e}"))?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    /// 锁住并访问底层连接。锁中毒(某处 panic)视为致命错误。
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().expect("db connection lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_in_memory_and_runs_sql() {
        let db = Db::open(":memory:").unwrap();
        db.lock()
            .execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
        let x: i64 = db.lock().query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(x, 1);
    }

    #[test]
    fn missing_file_directory_is_an_error() {
        assert!(Db::open("/nonexistent-dir/x.db").is_err());
    }
}
```

- [ ] **Step 3: 注册模块** —— `src/lib.rs` 的模块列表加 `pub mod db;`,层注释加一行:

```rust
//! * [`db`]        — shared SQLite infrastructure (connection management).
```

- [ ] **Step 4: 运行验证**

Run: `cargo test db`
Expected: `opens_in_memory_and_runs_sql ... ok`(2 passed)

- [ ] **Step 5: clippy + 提交**

```bash
cargo clippy --all-targets && git add Cargo.toml Cargo.lock src/db.rs src/lib.rs && git commit -m "Add shared SQLite db module

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: account wire 类型(记录/信息/视图)

**Files:**
- Create: `src/account/mod.rs`
- Create: `src/account/wire.rs`
- Modify: `src/lib.rs`(注册 `account`)

**Interfaces:**
- Consumes: 无
- Produces:
  - `AccountRecord { tag: Option<String>, user: Value, access_token: String, refresh_token: Option<String>, obtained_at: Option<String> }`(Deserialize;`email() -> Option<String>` 从 `user` JSON 提取)
  - `AccountInfo { tier, sub_status, trial_days_left, current_period_end, remaining_credits, total_credits, remaining_usd: Option<…>, next_credits_at: Option<String> }`(refresh 结果,`f64`/`i64` 可空字段)
  - `AccountView`(Serialize,`access_token` 已掩码,含全部额度字段与时间戳)
  - `mask_token(token: &str) -> String`(前 8 位 + `...`)
  - `From<crate::raycast::wire::{RaycastMe, RaycastCredits}> for AccountInfo`(Task 7 提供 me/credits 后使用;本任务先定义好结构)

- [ ] **Step 1: 模块与类型**

```rust
// src/account/mod.rs
//! 账号域:SQLite 存储的 Raycast 账号、启动导入、额度刷新。
//! 建在 [`crate::db`] 之上;不依赖 HTTP 层。

pub mod import;
pub mod refresh;
pub mod store;
pub mod wire;
```

注意:`import`/`refresh`/`store` 文件在后续任务才创建,本任务先在 `mod.rs` 里只声明 `pub mod wire;`,后续任务再追加声明(否则编译失败)。**本任务的 mod.rs 只写:**

```rust
//! 账号域:SQLite 存储的 Raycast 账号、启动导入、额度刷新。
//! 建在 [`crate::db`] 之上;不依赖 HTTP 层。

pub mod wire;
```

```rust
// src/account/wire.rs
//! 账号域的对外 JSON 形状:入站记录、额度信息、出站视图。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::raycast::wire::{RaycastCredits, RaycastMe};

/// account.json 的一条账号(也是 `POST /v1/accounts` 的请求体)。
/// `user` 保留完整原始对象(存 `user_json` 列),email 从中提取。
#[derive(Debug, Clone, Deserialize)]
pub struct AccountRecord {
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub user: Value,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub obtained_at: Option<String>,
}

impl AccountRecord {
    /// 唯一标识:`user.email`。
    pub fn email(&self) -> Option<String> {
        self.user.get("email")?.as_str().map(str::to_string)
    }
}

/// 一次额度刷新拉到的信息(来自上游 `/me` + `/ai/credits`)。
#[derive(Debug, Clone, Default)]
pub struct AccountInfo {
    pub tier: Option<String>,
    pub sub_status: Option<String>,
    pub trial_days_left: Option<i64>,
    pub current_period_end: Option<i64>,
    pub remaining_credits: Option<f64>,
    pub total_credits: Option<f64>,
    pub remaining_usd: Option<f64>,
    pub next_credits_at: Option<String>,
}

impl From<(RaycastMe, RaycastCredits)> for AccountInfo {
    fn from((me, credits): (RaycastMe, RaycastCredits)) -> Self {
        let sub = me.subscription;
        Self {
            tier: sub.as_ref().and_then(|s| s.tier.clone()),
            sub_status: sub.as_ref().and_then(|s| s.status.clone()),
            trial_days_left: sub.as_ref().and_then(|s| s.trial_days_left),
            current_period_end: sub.as_ref().and_then(|s| s.current_period_end),
            remaining_credits: credits.remaining_balance_credits,
            total_credits: credits.total_balance_credits,
            remaining_usd: credits.remaining_balance_usd,
            next_credits_at: credits.next_credits_at,
        }
    }
}

/// `GET /v1/accounts` 的一条响应:token 掩码,含额度与可用状态。
#[derive(Debug, Serialize)]
pub struct AccountView {
    pub email: String,
    pub tag: Option<String>,
    pub access_token: String,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub obtained_at: Option<String>,
    pub tier: Option<String>,
    pub sub_status: Option<String>,
    pub trial_days_left: Option<i64>,
    pub current_period_end: Option<i64>,
    pub remaining_credits: Option<f64>,
    pub total_credits: Option<f64>,
    pub remaining_usd: Option<f64>,
    pub next_credits_at: Option<String>,
    pub info_fetched_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 掩码:token 前 8 位 + `...`。
pub fn mask_token(token: &str) -> String {
    let prefix: String = token.chars().take(8).collect();
    format!("{prefix}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_extracts_email_from_user() {
        let rec: AccountRecord = serde_json::from_json(
            r#"{ "tag": "a", "user": { "email": "x@y.z", "id": 1 },
                "access_token": "rca_abc", "refresh_token": "r",
                "obtained_at": "2026-09-14T00:00:00Z" }"#,
        )
        .unwrap();
        assert_eq!(rec.email().as_deref(), Some("x@y.z"));
    }

    #[test]
    fn record_without_email_or_user_is_tolerant() {
        // user 缺失或没有 email 都返回 None,而不是报错。
        let rec: AccountRecord = serde_json::from_json(r#"{ "access_token": "t" }"#).unwrap();
        assert_eq!(rec.email(), None);
    }

    #[test]
    fn token_masking() {
        assert_eq!(mask_token("rca_1234567890abcdef"), "rca_1234...");
        assert_eq!(mask_token("short"), "short...");
    }
}
```

**注意**:`wire.rs` 引用了 `crate::raycast::wire::{RaycastMe, RaycastCredits}` —— 这两个类型 Task 7 才创建。为让本任务独立编译,**本任务先在 `src/raycast/wire.rs` 追加最小占位实现**(Task 7 再补字段细节):

```rust
// src/raycast/wire.rs 追加(文件末尾)
// --- 账号信息查询(Task 7 补充字段) -------------------------------

/// `GET /api/v1/me` 的瘦响应(本任务占位,Task 7 完整化)。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct RaycastMe {
    #[serde(default)]
    pub subscription: Option<RaycastSubscription>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct RaycastSubscription {
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub trial_days_left: Option<i64>,
    #[serde(default)]
    pub current_period_end: Option<i64>,
}

/// `GET /api/v1/ai/credits` 的瘦响应(本任务占位,Task 7 完整化)。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct RaycastCredits {
    #[serde(default)]
    pub remaining_balance_credits: Option<f64>,
    #[serde(default)]
    pub total_balance_credits: Option<f64>,
    #[serde(default)]
    pub remaining_balance_usd: Option<f64>,
    #[serde(default)]
    pub next_credits_at: Option<String>,
}
```

- [ ] **Step 2: lib.rs 注册**(`pub mod account;` + 层注释 `//! * [`account`]   — the Raycast account domain (store, import, refresh).`,按字母序放在 `config` 前)

- [ ] **Step 3: 运行验证**

Run: `cargo test account`
Expected: 3 passed(record×2 + mask)

- [ ] **Step 4: clippy + 提交**

```bash
cargo clippy --all-targets && git add src/account src/lib.rs src/raycast/wire.rs && git commit -m "Add account wire types (record, info, view)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: account store(建表 + CRUD + 轮询 + 摘除)

**Files:**
- Create: `src/account/store.rs`
- Modify: `src/account/mod.rs`(追加 `pub mod store;`)
- Modify: `src/error.rs`(加 `NoAccount` 变体)
- Modify: `tests/error_mapping.rs`(加状态断言)

**Interfaces:**
- Consumes: `Db`(Task 1)、`AccountRecord`/`AccountInfo`/`AccountView`(Task 2)
- Produces:
  - `UpsertOutcome { Inserted, Updated, Unchanged }`(PartialEq)
  - `AccountRow`(Clone;email/tag/access_token/refresh_token/obtained_at/available/unavailable_reason/额度 8 字段/info_fetched_at/created_at/updated_at)
  - `AccountStore::new(db: Db) -> AccountStore`(构造时建表)
  - `upsert(&self, &AccountRecord) -> Result<UpsertOutcome, GatewayError>`(email 缺失 → `BadRequest`)
  - `list(&self) -> Result<Vec<AccountRow>, GatewayError>`
  - `delete(&self, email: &str) -> Result<bool, GatewayError>`(false = 不存在)
  - `next_available(&self) -> Result<AccountRow, GatewayError>`(无可用 → `GatewayError::NoAccount`)
  - `mark_unavailable(&self, email: &str, reason: &str)`
  - `set_available(&self, email: &str, available: bool, reason: Option<&str>)`
  - `update_info(&self, email: &str, &AccountInfo) -> Result<(), GatewayError>`
  - `GatewayError::NoAccount(String)` → HTTP 503 / type `server_error`

- [ ] **Step 1: error.rs 加变体**(`Upstream` 变体后追加)

```rust
    /// No usable Raycast account in the store (503).
    NoAccount(String),
```

`parts()` 的 match 追加分支:

```rust
            GatewayError::NoAccount(m) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                m.clone(),
            ),
```

`tests/error_mapping.rs` 的状态测试追加一个 `#[test]`:

```rust
#[test]
fn no_account_maps_to_503() {
    assert_eq!(
        status_of(GatewayError::NoAccount("no available account".into())),
        StatusCode::SERVICE_UNAVAILABLE
    );
}
```

- [ ] **Step 2: 写 store 失败测试**(先写 `src/account/store.rs` 只含 `use` + 空 struct + tests,跑 `cargo test store` 确认编译失败)

测试代码(最终放在 `#[cfg(test)] mod tests`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::wire::AccountRecord;
    use crate::db::Db;

    fn rec(email: &str, token: &str) -> AccountRecord {
        serde_json::from_value(serde_json::json!({
            "tag": "t",
            "user": { "email": email },
            "access_token": token,
            "refresh_token": "r1"
        }))
        .unwrap()
    }

    fn store() -> AccountStore {
        AccountStore::new(Db::open(":memory:").unwrap())
    }

    #[test]
    fn upsert_three_states() {
        let s = store();
        // 插入
        assert_eq!(s.upsert(&rec("a@x", "t1")).unwrap(), UpsertOutcome::Inserted);
        // token 相同 → 跳过
        assert_eq!(s.upsert(&rec("a@x", "t1")).unwrap(), UpsertOutcome::Unchanged);
        // token 变化 → 更新
        assert_eq!(s.upsert(&rec("a@x", "t2")).unwrap(), UpsertOutcome::Updated);
        assert_eq!(s.list().unwrap()[0].access_token, "t2");
    }

    #[test]
    fn update_resets_availability() {
        let s = store();
        s.upsert(&rec("a@x", "t1")).unwrap();
        s.mark_unavailable("a@x", "token 失效 (401)");
        assert!(!s.list().unwrap()[0].available);
        // 带 token 更新 → 恢复
        s.upsert(&rec("a@x", "t9")).unwrap();
        let row = s.list().unwrap()[0].clone();
        assert!(row.available);
        assert_eq!(row.unavailable_reason, None);
    }

    #[test]
    fn upsert_without_email_is_bad_request() {
        let s = store();
        let bad: AccountRecord = serde_json::from_json(r#"{ "access_token": "t" }"#).unwrap();
        assert!(matches!(
            s.upsert(&bad).unwrap_err(),
            GatewayError::BadRequest(_)
        ));
    }

    #[test]
    fn rotation_cycles_and_skips_unavailable() {
        let s = store();
        s.upsert(&rec("a@x", "ta")).unwrap();
        s.upsert(&rec("b@x", "tb")).unwrap();
        s.upsert(&rec("c@x", "tc")).unwrap();
        assert_eq!(s.next_available().unwrap().email, "a@x");
        assert_eq!(s.next_available().unwrap().email, "b@x");
        // 摘除 b 后,轮到 c,再循环 a、c
        s.mark_unavailable("b@x", "额度耗尽");
        assert_eq!(s.next_available().unwrap().email, "c@x");
        assert_eq!(s.next_available().unwrap().email, "a@x");
        assert_eq!(s.next_available().unwrap().email, "c@x");
    }

    #[test]
    fn no_available_account_errors() {
        let s = store();
        assert!(matches!(
            s.next_available().unwrap_err(),
            GatewayError::NoAccount(_)
        ));
        s.upsert(&rec("a@x", "ta")).unwrap();
        s.mark_unavailable("a@x", "x");
        assert!(matches!(
            s.next_available().unwrap_err(),
            GatewayError::NoAccount(_)
        ));
    }

    #[test]
    fn delete_reports_existence() {
        let s = store();
        s.upsert(&rec("a@x", "ta")).unwrap();
        assert!(s.delete("a@x").unwrap());
        assert!(!s.delete("a@x").unwrap());
        assert!(s.list().unwrap().is_empty());
    }

    #[test]
    fn update_info_writes_quota_and_availability() {
        let s = store();
        s.upsert(&rec("a@x", "ta")).unwrap();
        let info = AccountInfo {
            tier: Some("pro".into()),
            sub_status: Some("trialing".into()),
            remaining_credits: Some(5.5),
            total_credits: Some(10.0),
            remaining_usd: Some(0.55),
            ..AccountInfo::default()
        };
        s.update_info("a@x", &info).unwrap();
        let row = s.list().unwrap()[0].clone();
        assert_eq!(row.tier.as_deref(), Some("pro"));
        assert_eq!(row.remaining_credits, Some(5.5));
        assert!(row.info_fetched_at.is_some());

        // set_available 独立操作
        s.set_available("a@x", false, Some("额度耗尽"));
        assert!(!s.list().unwrap()[0].available);
        s.set_available("a@x", true, None);
        assert!(s.list().unwrap()[0].available);
    }
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test store`
Expected: 编译失败(`UpsertOutcome`/`AccountStore` 未定义)

- [ ] **Step 4: 实现 store**

```rust
//! src/account/store.rs
//! 账号存储:accounts 表的 CRUD、轮询选号与可用性管理。

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::account::wire::{AccountInfo, AccountRecord, AccountView};
use crate::db::Db;
use crate::error::GatewayError;
use crate::util::unix_secs;

/// 一次 upsert 的三态结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    Inserted,
    Updated,
    Unchanged,
}

/// accounts 表的一行(含明文 token,仅在内部与 refresh 使用)。
#[derive(Debug, Clone)]
pub struct AccountRow {
    pub email: String,
    pub tag: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub obtained_at: Option<String>,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub tier: Option<String>,
    pub sub_status: Option<String>,
    pub trial_days_left: Option<i64>,
    pub current_period_end: Option<i64>,
    pub remaining_credits: Option<f64>,
    pub total_credits: Option<f64>,
    pub remaining_usd: Option<f64>,
    pub next_credits_at: Option<String>,
    pub info_fetched_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl AccountRow {
    /// 转为对外视图:token 掩码。
    pub fn to_view(&self) -> AccountView {
        AccountView {
            email: self.email.clone(),
            tag: self.tag.clone(),
            access_token: crate::account::wire::mask_token(&self.access_token),
            available: self.available,
            unavailable_reason: self.unavailable_reason.clone(),
            obtained_at: self.obtained_at.clone(),
            tier: self.tier.clone(),
            sub_status: self.sub_status.clone(),
            trial_days_left: self.trial_days_left,
            current_period_end: self.current_period_end,
            remaining_credits: self.remaining_credits,
            total_credits: self.total_credits,
            remaining_usd: self.remaining_usd,
            next_credits_at: self.next_credits_at.clone(),
            info_fetched_at: self.info_fetched_at.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    email               TEXT NOT NULL UNIQUE,
    tag                 TEXT,
    access_token        TEXT NOT NULL,
    refresh_token       TEXT,
    obtained_at         TEXT,
    user_json           TEXT,
    available           INTEGER NOT NULL DEFAULT 1,
    unavailable_reason  TEXT,
    tier                TEXT,
    sub_status          TEXT,
    trial_days_left     INTEGER,
    current_period_end  INTEGER,
    remaining_credits   REAL,
    total_credits       REAL,
    remaining_usd       REAL,
    next_credits_at     TEXT,
    info_fetched_at     TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);
";

/// 查询列的统一顺序(`list` / `next_available` 共用)。
const ROW_COLUMNS: &str = "email, tag, access_token, refresh_token, obtained_at, \
    available, unavailable_reason, tier, sub_status, trial_days_left, \
    current_period_end, remaining_credits, total_credits, remaining_usd, \
    next_credits_at, info_fetched_at, created_at, updated_at";

pub struct AccountStore {
    db: Db,
    /// 轮询游标;跨账号增删保持单调,取模即得下标。
    counter: AtomicUsize,
}

impl AccountStore {
    /// 构造并建表(幂等)。
    pub fn new(db: Db) -> Self {
        db.lock()
            .execute_batch(SCHEMA)
            .expect("create accounts schema");
        Self {
            db,
            counter: AtomicUsize::new(0),
        }
    }

    /// 按 email upsert;三态语义见 [`UpsertOutcome`]。
    pub fn upsert(&self, record: &AccountRecord) -> Result<UpsertOutcome, GatewayError> {
        let email = record
            .email()
            .ok_or_else(|| GatewayError::BadRequest("account entry has no user.email".into()))?;
        let user_json = serde_json::to_string(&record.user).unwrap_or_default();
        let now = unix_secs().to_string();
        let conn = self.db.lock();

        let existing: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT access_token, refresh_token FROM accounts WHERE email = ?1",
                [&email],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();

        match existing {
            None => {
                conn.execute(
                    "INSERT INTO accounts
                        (email, tag, access_token, refresh_token, obtained_at,
                         user_json, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                    rusqlite::params![
                        email,
                        record.tag,
                        record.access_token,
                        record.refresh_token,
                        record.obtained_at,
                        user_json,
                        now,
                    ],
                )
                .map_err(sql_err)?;
                Ok(UpsertOutcome::Inserted)
            }
            Some((access, refresh))
                if access != record.access_token || refresh != record.refresh_token =>
            {
                // token 变了:更新全部业务字段并恢复可用(新 token 可能已复活)。
                conn.execute(
                    "UPDATE accounts SET
                        tag = ?2, access_token = ?3, refresh_token = ?4,
                        obtained_at = ?5, user_json = ?6,
                        available = 1, unavailable_reason = NULL, updated_at = ?7
                     WHERE email = ?1",
                    rusqlite::params![
                        email,
                        record.tag,
                        record.access_token,
                        record.refresh_token,
                        record.obtained_at,
                        user_json,
                        now,
                    ],
                )
                .map_err(sql_err)?;
                Ok(UpsertOutcome::Updated)
            }
            Some(_) => Ok(UpsertOutcome::Unchanged),
        }
    }

    /// 全部账号(含不可用的;refresh 需要它们的 token)。
    pub fn list(&self) -> Result<Vec<AccountRow>, GatewayError> {
        let conn = self.db.lock();
        let mut stmt = conn
            .prepare(&format!("SELECT {ROW_COLUMNS} FROM accounts ORDER BY id"))
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], row_to_account)
            .map_err(sql_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_err)?;
        Ok(rows)
    }

    /// 删除;返回是否存在。
    pub fn delete(&self, email: &str) -> Result<bool, GatewayError> {
        let conn = self.db.lock();
        let n = conn
            .execute("DELETE FROM accounts WHERE email = ?1", [email])
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    /// 轮询取下一个可用账号;全不可用报 [`GatewayError::NoAccount`]。
    pub fn next_available(&self) -> Result<AccountRow, GatewayError> {
        let rows = {
            let conn = self.db.lock();
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {ROW_COLUMNS} FROM accounts WHERE available = 1 ORDER BY id"
                ))
                .map_err(sql_err)?;
            stmt.query_map([], row_to_account)
                .map_err(sql_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_err)?
        };
        if rows.is_empty() {
            return Err(GatewayError::NoAccount(
                "no available Raycast account; add or refresh one via POST /v1/accounts".into(),
            ));
        }
        let idx = self.counter.fetch_add(1, Ordering::Relaxed);
        Ok(rows[idx % rows.len()].clone())
    }

    /// 摘除(401 / 额度耗尽)。
    pub fn mark_unavailable(&self, email: &str, reason: &str) {
        self.set_available(email, false, Some(reason));
    }

    /// 设置可用性;`available = true` 时清空原因。
    pub fn set_available(&self, email: &str, available: bool, reason: Option<&str>) {
        let now = unix_secs().to_string();
        let conn = self.db.lock();
        let _ = conn.execute(
            "UPDATE accounts
                SET available = ?2,
                    unavailable_reason = ?3,
                    updated_at = ?4
             WHERE email = ?1",
            rusqlite::params![email, available, reason, now],
        );
    }

    /// 写入额度/订阅信息与 `info_fetched_at`。
    pub fn update_info(&self, email: &str, info: &AccountInfo) -> Result<(), GatewayError> {
        let now = unix_secs().to_string();
        let conn = self.db.lock();
        conn.execute(
            "UPDATE accounts SET
                tier = ?2, sub_status = ?3, trial_days_left = ?4,
                current_period_end = ?5, remaining_credits = ?6, total_credits = ?7,
                remaining_usd = ?8, next_credits_at = ?9,
                info_fetched_at = ?10, updated_at = ?10
             WHERE email = ?1",
            rusqlite::params![
                email,
                info.tier,
                info.sub_status,
                info.trial_days_left,
                info.current_period_end,
                info.remaining_credits,
                info.total_credits,
                info.remaining_usd,
                info.next_credits_at,
                now,
            ],
        )
        .map_err(sql_err)?;
        Ok(())
    }
}

/// 行映射(列顺序与 `ROW_COLUMNS` 一致)。
fn row_to_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRow> {
    Ok(AccountRow {
        email: r.get(0)?,
        tag: r.get(1)?,
        access_token: r.get(2)?,
        refresh_token: r.get(3)?,
        obtained_at: r.get(4)?,
        available: r.get::<_, i64>(5)? != 0,
        unavailable_reason: r.get(6)?,
        tier: r.get(7)?,
        sub_status: r.get(8)?,
        trial_days_left: r.get(9)?,
        current_period_end: r.get(10)?,
        remaining_credits: r.get(11)?,
        total_credits: r.get(12)?,
        remaining_usd: r.get(13)?,
        next_credits_at: r.get(14)?,
        info_fetched_at: r.get(15)?,
        created_at: r.get(16)?,
        updated_at: r.get(17)?,
    })
}

/// SQL 错误统一进 `GatewayError::Internal`。
fn sql_err(e: rusqlite::Error) -> GatewayError {
    GatewayError::Internal(format!("account store: {e}"))
}
```

`src/account/mod.rs` 追加 `pub mod store;`。

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test account && cargo test error_mapping`
Expected: account 7 passed;error_mapping 全绿(含新 no_account)

- [ ] **Step 6: clippy + 提交**

```bash
cargo clippy --all-targets && git add src/account/store.rs src/account/mod.rs src/error.rs tests/error_mapping.rs && git commit -m "Add account store with upsert, rotation, and availability

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: account import(启动导入 account.json)

**Files:**
- Create: `src/account/import.rs`
- Modify: `src/account/mod.rs`(追加 `pub mod import;`)

**Interfaces:**
- Consumes: `AccountStore::upsert`(Task 3)
- Produces: `import_file(store: &AccountStore, path: &str) -> Result<ImportSummary, GatewayError>`
  - `ImportSummary { inserted: usize, updated: usize, unchanged: usize, skipped: usize }`(Debug/PartialEq)
  - 文件不存在 → `Ok(ImportSummary::default())`(静默);文件存在但 JSON 非法 → `Err(GatewayError::Config(...))`

- [ ] **Step 1: 写失败测试**

```rust
// src/account/import.rs
//! 启动导入:account.json → 账号表。

use crate::account::store::{AccountStore, UpsertOutcome};
use crate::error::GatewayError;

/// 一次导入的计数。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// email 缺失等被跳过的条目数。
    pub skipped: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use std::path::PathBuf;

    fn tmp_file(name: &str, content: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("r2a-import-test-{name}-{}.json", std::process::id()));
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn imports_and_upserts_by_email() {
        let store = AccountStore::new(Db::open(":memory:").unwrap());
        let file = tmp_file(
            "basic",
            r#"[{ "tag": "a", "user": { "email": "a@x" }, "access_token": "t1" },
                { "tag": "b", "user": { "email": "b@x" }, "access_token": "t2" }]"#,
        );
        let s = import_file(&store, file.to_str().unwrap()).unwrap();
        assert_eq!(s, ImportSummary { inserted: 2, ..Default::default() });

        // token 变化 → 更新;相同 → 跳过;无 email → skipped
        let file2 = tmp_file(
            "second",
            r#"[{ "tag": "a", "user": { "email": "a@x" }, "access_token": "t9" },
                { "tag": "b", "user": { "email": "b@x" }, "access_token": "t2" },
                { "tag": "bad", "access_token": "t3" }]"#,
        );
        let s = import_file(&store, file2.to_str().unwrap()).unwrap();
        assert_eq!(
            s,
            ImportSummary { inserted: 0, updated: 1, unchanged: 1, skipped: 1 }
        );
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&file2);
    }

    #[test]
    fn missing_file_is_silent_noop() {
        let store = AccountStore::new(Db::open(":memory:").unwrap());
        let s = import_file(&store, "/nonexistent/account.json").unwrap();
        assert_eq!(s, ImportSummary::default());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn malformed_file_is_config_error() {
        let store = AccountStore::new(Db::open(":memory:").unwrap());
        let file = tmp_file("bad", "not json");
        assert!(matches!(
            import_file(&store, file.to_str().unwrap()).unwrap_err(),
            GatewayError::Config(_)
        ));
        let _ = std::fs::remove_file(&file);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test import`
Expected: 编译失败(`import_file` 未定义)

- [ ] **Step 3: 实现**(在 tests mod 之前插入)

```rust
use crate::account::wire::AccountRecord;

/// 从 `path` 导入账号;文件缺失静默返回零计数。
pub fn import_file(store: &AccountStore, path: &str) -> Result<ImportSummary, GatewayError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        // 没有账号文件是合法状态(账号可以只来自 POST /v1/accounts)。
        Err(_) => return Ok(ImportSummary::default()),
    };

    let records: Vec<AccountRecord> = serde_json::from_str(&content)
        .map_err(|e| GatewayError::Config(format!("failed to parse '{path}': {e}")))?;

    let mut summary = ImportSummary::default();
    for record in records {
        if record.email().is_none() {
            tracing::warn!(tag = ?record.tag, "account entry has no user.email, skipped");
            summary.skipped += 1;
            continue;
        }
        match store.upsert(&record)? {
            UpsertOutcome::Inserted => summary.inserted += 1,
            UpsertOutcome::Updated => summary.updated += 1,
            UpsertOutcome::Unchanged => summary.unchanged += 1,
        }
    }
    tracing::info!(
        inserted = summary.inserted,
        updated = summary.updated,
        unchanged = summary.unchanged,
        skipped = summary.skipped,
        "account import from '{path}'"
    );
    Ok(summary)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test import`
Expected: 3 passed

- [ ] **Step 5: clippy + 提交**

```bash
cargo clippy --all-targets && git add src/account && git commit -m "Add account.json startup import

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: client 切换到账号池(轮询 + 401 摘除)

**Files:**
- Modify: `src/raycast/client.rs`
- Modify: `src/main.rs`(最小接线:开库、建 store、导入、传给 client)

**Interfaces:**
- Consumes: `AccountStore`(Task 3)、`account::import::import_file`(Task 4)
- Produces:
  - `RaycastClient::new(http: Client, settings: Arc<Settings>, accounts: Arc<AccountStore>) -> Self`
  - `RaycastClient::accounts(&self) -> &Arc<AccountStore>`(refresh 与测试用)
  - `send_chat`/`fetch_models` 行为:先 `next_available()`(无账号 → 503 错误),非 2xx 且 status == 401 → `mark_unavailable(email, "token 失效 (401)")` 后返回原错误
  - `base_headers` 签名变为 `(&self, bearer: &str, timestamp: Option<&str>, signature: Option<&str>)`(私有)

**注意**:`settings.raycast_bearer_token` 从此不再被读取,但字段保留到 Task 6 再删(config 一次清理)。此期间 main 仍从 config 拿到它,只是没人用 —— 编译器不会警告(结构体字段)。

- [ ] **Step 1: 改 client.rs**

结构体与新构造:

```rust
use std::sync::Arc;

use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};

use crate::account::store::AccountStore;
use crate::config::{RAYCAST_API_URL, RAYCAST_MODELS_URL, Settings, USER_AGENT};
use crate::debug;
use crate::error::GatewayError;
use crate::signature::calculate_signature_v2;
use crate::util::unix_secs;

use super::wire::{RaycastChatRequest, RaycastModelsResponse, RaycastRawModel};

/// Cheap-to-clone handle to the Raycast backend (wraps a shared `reqwest::Client`).
#[derive(Clone)]
pub struct RaycastClient {
    http: Client,
    settings: Arc<Settings>,
    /// 账号池:每次请求轮询取一个可用账号的 token。
    accounts: Arc<AccountStore>,
}

impl RaycastClient {
    pub fn new(
        http: Client,
        settings: Arc<Settings>,
        accounts: Arc<AccountStore>,
    ) -> Self {
        Self {
            http,
            settings,
            accounts,
        }
    }

    /// 账号池(刷新任务与测试使用)。
    pub fn accounts(&self) -> &Arc<AccountStore> {
        &self.accounts
    }
```

`base_headers` 的 bearer 一行改为参数:

```rust
    fn base_headers(
        &self,
        bearer: &str,
        timestamp: Option<&str>,
        signature: Option<&str>,
    ) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let bearer = format!("Bearer {bearer}");
```

`send_chat` 全量替换为:

```rust
    /// POST a chat request. Returns the streaming response on success (status
    /// already validated); the caller consumes `bytes_stream()`.
    pub async fn send_chat(
        &self,
        request: &RaycastChatRequest,
    ) -> Result<reqwest::Response, GatewayError> {
        let account = self.accounts.next_available()?;
        let body = serde_json::to_string(request)?;
        let timestamp = unix_secs().to_string();
        let signature = calculate_signature_v2(
            &timestamp,
            &self.settings.raycast_device_id,
            &body,
            self.settings.signature_secret(),
        );

        let mut headers = self.base_headers(&account.access_token, Some(&timestamp), Some(&signature));
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert("Accept", HeaderValue::from_static("text/event-stream"));

        // Dump the outgoing upstream body for offline analysis, enabled by
        // DEBUG_DUMP_DIR and correlated to the inbound request via the
        // task-local id set by `debug_capture`.
        let body_value = debug::body_to_value(body.as_bytes());
        if let Ok(text) = serde_json::to_string_pretty(&body_value) {
            debug::dump(
                &format!("{}-upstream.json", debug::current_request_id()),
                &text,
            );
        }

        let response = self
            .http
            .post(RAYCAST_API_URL)
            .headers(headers)
            .body(body)
            .send()
            .await?;

        match ensure_success(response).await {
            Ok(response) => Ok(response),
            // 401 = 该账号 token 失效:摘除后返回错误,下次请求自动换号。
            Err(err @ GatewayError::Upstream { status: 401, .. }) => {
                self.accounts
                    .mark_unavailable(&account.email, "token 失效 (401)");
                Err(err)
            }
            Err(err) => Err(err),
        }
    }
```

`fetch_models` 全量替换为:

```rust
    /// Fetch the raw list of models available to the account.
    pub async fn fetch_models(&self) -> Result<Vec<RaycastRawModel>, GatewayError> {
        let account = self.accounts.next_available()?;
        let mut headers = self.base_headers(&account.access_token, None, None);
        headers.insert("Accept", HeaderValue::from_static("application/json"));

        let response = self
            .http
            .get(RAYCAST_MODELS_URL)
            .headers(headers)
            .send()
            .await?;

        let response = ensure_success(response).await?;
        let parsed: RaycastModelsResponse = response.json().await?;
        Ok(parsed.models)
    }
```

- [ ] **Step 2: main.rs 最小接线**(Task 6 再换 settings.accounts_db;此处硬编码默认路径)

把构造段替换为:

```rust
    let db = raycast2api::db::Db::open("accounts.db")
        .unwrap_or_else(|e| {
            eprintln!("accounts db error: {e}");
            std::process::exit(1);
        });
    let accounts = Arc::new(raycast2api::account::store::AccountStore::new(db));
    if let Err(err) = raycast2api::account::import::import_file(&accounts, "account.json") {
        eprintln!("account import error: {err}");
        std::process::exit(1);
    }

    let client = RaycastClient::new(http, settings.clone(), accounts);
    let state = Arc::new(AppState::new(settings.clone(), client));
```

- [ ] **Step 3: 验证编译与既有测试**(client 无独立单测;请求级行为由 Task 9 的契约测试覆盖)

Run: `cargo test`
Expected: 全部通过(既有 13 个套件)

- [ ] **Step 4: clippy + 提交**

```bash
cargo clippy --all-targets && git add src/raycast/client.rs src/main.rs && git commit -m "Switch upstream client to rotating account pool

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: config 清理(移除单账号必填,加 accounts_db)

**Files:**
- Modify: `src/config.rs`
- Modify: `src/main.rs`(用 `settings.accounts_db` 开库;更新报错文案)

**Interfaces:**
- Consumes: 无
- Produces: `Settings { raycast_device_id, raycast_signature_secret, api_key, bind_addr, bind_addr_dual, show_advanced, include_deprecated, accounts_db: String }`(不再有 `raycast_bearer_token`);`ConfigFile` 删 `raycast_bearer_token`,加 `accounts_db: Option<String>`;env 无 `RAYCAST_BEARER_TOKEN`。

- [ ] **Step 1: 改 config.rs**

`ConfigFile`:

```rust
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConfigFile {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub api_key: Option<String>,
    pub raycast_device_id: Option<String>,
    pub raycast_signature_secret: Option<String>,
    pub show_advanced: Option<bool>,
    pub include_deprecated: Option<bool>,
    /// SQLite 账号库路径,默认 `accounts.db`。
    pub accounts_db: Option<String>,
}
```

`Settings` 删 `raycast_bearer_token` 字段及其 doc,追加:

```rust
    /// SQLite 账号库路径(账号表与后续模块共用)。
    pub accounts_db: String,
```

`from_config_file` 删 bearer 段,加:

```rust
        let accounts_db = env_opt("ACCOUNTS_DB")
            .or(config.accounts_db)
            .unwrap_or_else(|| "accounts.db".to_string());
```

构造体里 `raycast_bearer_token,` 行删除,`accounts_db,` 加入。`from_env` 同样删 bearer,加同样的 `accounts_db`(env → 默认),构造体同步。两处方法末尾的 `Ok(Self { ... })` 均相应调整。

- [ ] **Step 2: main.rs**

开库行改为:

```rust
    let db = raycast2api::db::Db::open(&settings.accounts_db)
```

启动报错文案改为:

```rust
                "Create a config.json file or set RAYCAST_DEVICE_ID environment variables (see .env.example)."
```

- [ ] **Step 3: 验证**

Run: `cargo test && cargo clippy --all-targets`
Expected: 全绿、零警告(`raycast_bearer_token` 已无引用)

- [ ] **Step 4: 提交**

```bash
git add src/config.rs src/main.rs && git commit -m "Drop single-account bearer config; add accounts_db setting

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: raycast me/credits 查询接口

**Files:**
- Modify: `src/raycast/wire.rs`(把 Task 2 的占位类型补完整:credits 数字字段用 string-or-number 弹性反序列化)
- Modify: `src/raycast/client.rs`(加 `fetch_me`/`fetch_credits`)
- Modify: `src/config.rs`(加两个 URL 常量)

**Interfaces:**
- Consumes: 无新依赖
- Produces:
  - `config::RAYCAST_ME_URL = "https://backend.raycast.com/api/v1/me"`、`config::RAYCAST_AI_CREDITS_URL = "https://backend.raycast.com/api/v1/ai/credits"`
  - `RaycastClient::fetch_me(&self, token: &str) -> Result<RaycastMe, GatewayError>`
  - `RaycastClient::fetch_credits(&self, token: &str) -> Result<RaycastCredits, GatewayError>`

- [ ] **Step 1: config.rs 常量**(`RAYCAST_MODELS_URL` 后追加)

```rust
/// Endpoint returning the authenticated account's profile + subscription.
pub const RAYCAST_ME_URL: &str = "https://backend.raycast.com/api/v1/me";

/// Endpoint returning the account's AI credit balance.
pub const RAYCAST_AI_CREDITS_URL: &str = "https://backend.raycast.com/api/v1/ai/credits";
```

- [ ] **Step 2: wire.rs 补完整**(替换 Task 2 的占位块;credits 数值字段可能以字符串下发 —— 逆向脚本里用了 `parseFloat`)

```rust
// --- 账号信息查询(/me + /ai/credits) --------------------------------

/// `GET /api/v1/me` 的瘦响应:只取订阅相关字段。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RaycastMe {
    #[serde(default)]
    pub subscription: Option<RaycastSubscription>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RaycastSubscription {
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub trial_days_left: Option<i64>,
    #[serde(default)]
    pub current_period_end: Option<i64>,
}

/// `GET /api/v1/ai/credits` 的瘦响应。余额字段可能以数字或字符串下发。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RaycastCredits {
    #[serde(default, deserialize_with = "string_or_number_f64")]
    pub remaining_balance_credits: Option<f64>,
    #[serde(default, deserialize_with = "string_or_number_f64")]
    pub total_balance_credits: Option<f64>,
    #[serde(default, deserialize_with = "string_or_number_f64")]
    pub remaining_balance_usd: Option<f64>,
    #[serde(default)]
    pub next_credits_at: Option<String>,
}

/// 接受 JSON number 或可解析的字符串,统一为 `Option<f64>`。
fn string_or_number_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }))
}

#[cfg(test)]
mod credits_tests {
    use super::*;

    #[test]
    fn credits_parse_number_and_string_forms() {
        let c: RaycastCredits = serde_json::from_json(
            r#"{ "remaining_balance_credits": "5.5", "total_balance_credits": 10,
                "remaining_balance_usd": "0.55", "next_credits_at": "2026-10-01T00:00:00Z" }"#,
        )
        .unwrap();
        assert_eq!(c.remaining_balance_credits, Some(5.5));
        assert_eq!(c.total_balance_credits, Some(10.0));
        assert_eq!(c.remaining_balance_usd, Some(0.55));
        assert_eq!(c.next_credits_at.as_deref(), Some("2026-10-01T00:00:00Z"));
    }
}
```

- [ ] **Step 3: client.rs 加方法**(`fetch_models` 后追加;两个方法都接受显式 token —— refresh 要遍历所有账号,包括当前不可用的)

```rust
    /// Fetch the account profile + subscription for an explicit token.
    pub async fn fetch_me(&self, token: &str) -> Result<RaycastMe, GatewayError> {
        self.get_json(RAYCAST_ME_URL, token).await
    }

    /// Fetch the AI credit balance for an explicit token.
    pub async fn fetch_credits(&self, token: &str) -> Result<RaycastCredits, GatewayError> {
        self.get_json(RAYCAST_AI_CREDITS_URL, token).await
    }

    /// Shared bearer-only GET returning parsed JSON.
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        token: &str,
    ) -> Result<T, GatewayError> {
        let mut headers = self.base_headers(token, None, None);
        headers.insert("Accept", HeaderValue::from_static("application/json"));
        let response = self.http.get(url).headers(headers).send().await?;
        let response = ensure_success(response).await?;
        Ok(response.json().await?)
    }
```

imports 更新:`use crate::config::{RAYCAST_AI_CREDITS_URL, RAYCAST_API_URL, RAYCAST_ME_URL, RAYCAST_MODELS_URL, Settings, USER_AGENT};`,`use super::wire::{RaycastChatRequest, RaycastCredits, RaycastMe, RaycastModelsResponse, RaycastRawModel};`

- [ ] **Step 4: 验证 + 提交**

Run: `cargo test credits && cargo test`
Expected: credits 1 passed;全部通过

```bash
cargo clippy --all-targets && git add src/config.rs src/raycast && git commit -m "Add me/credits account-info fetch endpoints

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: refresh(额度刷新逻辑)

**Files:**
- Create: `src/account/refresh.rs`
- Modify: `src/account/mod.rs`(追加 `pub mod refresh;`)

**Interfaces:**
- Consumes: `RaycastClient::fetch_me/fetch_credits`(Task 7)、`AccountStore::{list, update_info, set_available}`(Task 3)
- Produces:
  - `pub const REFRESH_INTERVAL: std::time::Duration`(6 小时)
  - `pub async fn refresh_all(store: &Arc<AccountStore>, client: &RaycastClient)`
  - `pub enum AvailabilityDecision { MarkUnavailable(&'static str), MarkAvailable, Keep }` + `pub fn decide(info: &AccountInfo) -> AvailabilityDecision`(纯函数,单测覆盖)

- [ ] **Step 1: 写失败测试**(先建文件含 tests,`decide` 未实现 → 编译失败)

```rust
// src/account/refresh.rs 末尾
#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::wire::AccountInfo;

    fn info(remaining: Option<f64>) -> AccountInfo {
        AccountInfo {
            remaining_credits: remaining,
            ..AccountInfo::default()
        }
    }

    #[test]
    fn zero_or_negative_credits_mark_unavailable() {
        assert!(matches!(
            decide(&info(Some(0.0))),
            AvailabilityDecision::MarkUnavailable(_)
        ));
        assert!(matches!(
            decide(&info(Some(-1.0))),
            AvailabilityDecision::MarkUnavailable(_)
        ));
    }

    #[test]
    fn positive_credits_restore() {
        assert_eq!(decide(&info(Some(0.01))), AvailabilityDecision::MarkAvailable);
    }

    #[test]
    fn unknown_credits_keep_state() {
        assert_eq!(decide(&info(None)), AvailabilityDecision::Keep);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test refresh`
Expected: 编译失败

- [ ] **Step 3: 实现**

```rust
//! src/account/refresh.rs
//! 账号额度刷新:遍历账号调上游 /me + /ai/credits,结果入库并调整可用性。
//!
//! 由 `main` 装配调用(避免 raycast ↔ account 循环依赖):启动立即一遍,
//! 之后每 [`REFRESH_INTERVAL`] 一遍;`POST /v1/accounts/refresh` 也可手动触发。

use std::sync::Arc;
use std::time::Duration;

use crate::account::store::AccountStore;
use crate::account::wire::AccountInfo;
use crate::error::GatewayError;
use crate::raycast::client::RaycastClient;

/// 定时刷新间隔(6 小时)。
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// 刷新后一个账号的可用性决策。
#[derive(Debug, Clone, PartialEq)]
pub enum AvailabilityDecision {
    /// 摘除,携带原因(额度耗尽)。
    MarkUnavailable(&'static str),
    /// 恢复可用(查询成功即证明 token 有效,且有余额)。
    MarkAvailable,
    /// 额度未知,不动当前状态。
    Keep,
}

/// 纯决策:剩余额度 <= 0 摘除;> 0 恢复;未知保持。
pub fn decide(info: &AccountInfo) -> AvailabilityDecision {
    match info.remaining_credits {
        Some(remaining) if remaining <= 0.0 => {
            AvailabilityDecision::MarkUnavailable("额度耗尽")
        }
        Some(_) => AvailabilityDecision::MarkAvailable,
        None => AvailabilityDecision::Keep,
    }
}

/// 刷新所有账号(含不可用的 —— 它们的 token 可能已恢复)。单个账号
/// 失败只记日志,不影响其余。
pub async fn refresh_all(store: &Arc<AccountStore>, client: &RaycastClient) {
    let accounts = match store.list() {
        Ok(accounts) => accounts,
        Err(err) => {
            tracing::error!("account refresh: list failed: {err}");
            return;
        }
    };

    for account in accounts {
        match fetch_info(client, &account.access_token).await {
            Ok(Some(info)) => {
                if let Err(err) = store.update_info(&account.email, &info) {
                    tracing::warn!("account refresh: {} update failed: {err}", account.email);
                    continue;
                }
                match decide(&info) {
                    AvailabilityDecision::MarkUnavailable(reason) => {
                        store.mark_unavailable(&account.email, reason);
                    }
                    AvailabilityDecision::MarkAvailable => {
                        store.set_available(&account.email, true, None);
                    }
                    AvailabilityDecision::Keep => {}
                }
            }
            // 401:token 失效,摘除。
            Ok(None) => {
                store.mark_unavailable(&account.email, "token 失效 (401)");
            }
            // 其他错误:不动状态,已记日志。
            Err(()) => {}
        }
    }
    tracing::info!("account refresh: {} accounts processed", accounts.len());
}

/// 拉一个账号的 me + credits。`Ok(None)` = 401;`Err(())` = 其他错误(已记日志)。
async fn fetch_info(client: &RaycastClient, token: &str) -> Result<Option<AccountInfo>, ()> {
    let me = match client.fetch_me(token).await {
        Ok(me) => me,
        Err(GatewayError::Upstream { status: 401, .. }) => return Ok(None),
        Err(err) => {
            tracing::warn!("account refresh: /me failed: {err}");
            return Err(());
        }
    };
    let credits = match client.fetch_credits(token).await {
        Ok(credits) => credits,
        Err(GatewayError::Upstream { status: 401, .. }) => return Ok(None),
        Err(err) => {
            tracing::warn!("account refresh: /ai/credits failed: {err}");
            return Err(());
        }
    };
    Ok(Some(AccountInfo::from((me, credits))))
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test refresh`
Expected: 3 passed

- [ ] **Step 5: clippy + 提交**

```bash
cargo clippy --all-targets && git add src/account && git commit -m "Add account quota refresh with availability decisions

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 9: HTTP 路由 + handler + 契约测试

**Files:**
- Create: `src/server/handlers/accounts.rs`
- Modify: `src/server/handlers/mod.rs`(`pub mod accounts;`)
- Modify: `src/server/mod.rs`(路由)
- Modify: `src/server/state.rs`(`AppState.accounts` 字段)
- Modify: `src/main.rs`(state 构造传 store)
- Create: `tests/accounts_contract.rs`

**Interfaces:**
- Consumes: `AccountStore`(Task 3)、`import_file` 语义由 `upsert` 承担、`refresh_all`(Task 8)
- Produces(路由,均在 `require_api_key` 层内):
  - `GET /v1/accounts` → `Json<Vec<AccountView>>`
  - `POST /v1/accounts`(body = `AccountRecord`)→ `Json<UpsertResponse>`(`{ "result": "inserted" | "updated" | "unchanged" }`)
  - `POST /v1/accounts/refresh` → `202 Accepted`(后台执行)
  - `DELETE /v1/accounts/{email}` → `204` / `404`

- [ ] **Step 1: state.rs 加字段**

```rust
pub struct AppState {
    pub settings: Arc<Settings>,
    pub client: RaycastClient,
    pub health: BackendHealth,
    /// 账号池(账号 CRUD 接口与刷新触发使用)。
    pub accounts: Arc<AccountStore>,
    models_cache: RwLock<Option<ModelsCache>>,
}
```

`new` 签名与构造:

```rust
    pub fn new(
        settings: Arc<Settings>,
        client: RaycastClient,
        accounts: Arc<AccountStore>,
    ) -> Self {
        Self {
            settings,
            client,
            health: BackendHealth::default(),
            accounts,
            models_cache: RwLock::new(None),
        }
    }
```

imports 加 `use crate::account::store::AccountStore;`。`main.rs` 的 `AppState::new(settings.clone(), client)` 改为 `AppState::new(settings.clone(), client.clone(), accounts)`(client 需要 `.clone()`,因为后面 refresh 循环还要用 —— 本任务先 `client.clone()`,Task 10 调整所有权)。

- [ ] **Step 2: handler**

```rust
//! src/server/handlers/accounts.rs
//! `/v1/accounts` — 账号池的查看、新增/更新、删除与手动额度刷新。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::account::store::{AccountStore, UpsertOutcome};
use crate::account::wire::{AccountRecord, AccountView};
use crate::error::GatewayError;
use crate::server::state::SharedState;

/// `POST /v1/accounts` 的结果回执。
#[derive(Serialize)]
pub struct UpsertResponse {
    pub result: &'static str,
}

/// `GET /v1/accounts` — 全部账号(token 掩码)。
pub async fn list(
    State(state): State<SharedState>,
) -> Result<Json<Vec<AccountView>>, GatewayError> {
    let views = state
        .accounts
        .list()?
        .iter()
        .map(|row| row.to_view())
        .collect();
    Ok(Json(views))
}

/// `POST /v1/accounts` — 新增或更新一个账号(按 email upsert)。
pub async fn upsert(
    State(state): State<SharedState>,
    Json(record): Json<AccountRecord>,
) -> Result<Json<UpsertResponse>, GatewayError> {
    let result = match state.accounts.upsert(&record)? {
        UpsertOutcome::Inserted => "inserted",
        UpsertOutcome::Updated => "updated",
        UpsertOutcome::Unchanged => "unchanged",
    };
    Ok(Json(UpsertResponse { result }))
}

/// `DELETE /v1/accounts/{email}` — 删除;不存在 404。
pub async fn delete(
    State(state): State<SharedState>,
    Path(email): Path<String>,
) -> Result<StatusCode, GatewayError> {
    if state.accounts.delete(&email)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(GatewayError::NotFound(format!("no account with email {email}")))
    }
}

/// `POST /v1/accounts/refresh` — 手动触发一次全量额度刷新(后台执行)。
pub async fn refresh(State(state): State<SharedState>) -> Response {
    let accounts: Arc<AccountStore> = state.accounts.clone();
    let client = state.client.clone();
    tokio::spawn(async move {
        crate::account::refresh::refresh_all(&accounts, &client).await;
    });
    StatusCode::ACCEPTED.into_response()
}
```

`src/server/handlers/mod.rs` 加 `pub mod accounts;`(字母序在 `chat` 后)。

- [ ] **Step 3: 路由**(`src/server/mod.rs` 的 `inference` 定义之前、`api` 链上追加;`delete` 需要 `use axum::http::Method` 已有的 `Method::GET` 列表加 `Method::DELETE`)

```rust
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS, Method::DELETE])
        .allow_headers(Any)
        .max_age(std::time::Duration::from_secs(86_400));

    // 账号池管理(挂 API key 层内)。
    let api = Router::new()
        .route(
            "/v1/accounts",
            get(handlers::accounts::list).post(handlers::accounts::upsert),
        )
        .route("/v1/accounts/refresh", post(handlers::accounts::refresh))
        .route("/v1/accounts/{email}", delete(handlers::accounts::delete))
        .route(
            "/v1/chat/completions",
            post(handlers::chat::chat_completions),
        )
        .route("/v1/messages", post(handlers::messages::messages))
        .route("/v1/responses", post(handlers::responses::responses))
        .route("/v1/models", get(handlers::models::list_models))
        .route("/health", get(handlers::health::health))
        .route("/ready", get(handlers::health::ready))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::require_api_key,
        ))
        .layer(axum::middleware::from_fn(middleware::debug_capture));
```

(即:把原来 `inference` 与 `api` 两段合并成上面这一段 —— 账号路由与推理路由同层。`debug_capture` 移到最内层,推理与账号 POST 都会被捕获,但 `debug_capture` 不依赖 state,位置不变语义不变。)

- [ ] **Step 4: 契约测试**(创建 `tests/accounts_contract.rs`)

```rust
//! `/v1/accounts` CRUD 的契约测试(内存库;refresh 端点只验证 202)。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use raycast2api::account::store::AccountStore;
use raycast2api::config::Settings;
use raycast2api::db::Db;
use raycast2api::server::build_router;
use serde_json::Value;
use tower::ServiceExt;

fn app() -> (axum::Router, Arc<AccountStore>) {
    let store = Arc::new(AccountStore::new(Db::open(":memory:").unwrap()));
    let settings = Arc::new(Settings {
        raycast_device_id: "device".into(),
        raycast_signature_secret: None,
        api_key: Some("test-key".into()),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        bind_addr_dual: None,
        show_advanced: true,
        include_deprecated: true,
        accounts_db: ":memory:".into(),
    });
    let client = raycast2api::raycast::client::RaycastClient::new(
        reqwest::Client::new(),
        settings.clone(),
        store.clone(),
    );
    let state = Arc::new(raycast2api::server::state::AppState::new(
        settings,
        client,
        store.clone(),
    ));
    (build_router(state), store)
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    builder = builder.header("authorization", "Bearer test-key");
    let request = builder.body(Body::from(body.map(|b| b.to_string()).unwrap_or_default())).unwrap();
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

fn account_body(email: &str, token: &str) -> Value {
    serde_json::json!({
        "tag": "t",
        "user": { "email": email },
        "access_token": token
    })
}

#[tokio::test]
async fn list_upsert_delete_round_trip() {
    let (app, _store) = app();

    // 空列表
    let (status, body) = call(&app, Method::GET, "/v1/accounts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);

    // 新增
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/accounts",
        Some(account_body("a@x", "rca_token123")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"], "inserted");

    // 重复同 token → unchanged
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/accounts",
        Some(account_body("a@x", "rca_token123")),
    )
    .await;
    assert_eq!(body["result"], "unchanged");

    // token 变化 → updated
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/accounts",
        Some(account_body("a@x", "rca_token999")),
    )
    .await;
    assert_eq!(body["result"], "updated");

    // 列表:token 掩码
    let (status, body) = call(&app, Method::GET, "/v1/accounts", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["email"], "a@x");
    assert_eq!(arr[0]["access_token"], "rca_toke...");
    assert_eq!(arr[0]["available"], true);

    // 删除 → 204;再删 → 404
    let (status, _) = call(&app, Method::DELETE, "/v1/accounts/a@x", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&app, Method::DELETE, "/v1/accounts/a@x", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn refresh_endpoint_returns_accepted() {
    let (app, _store) = app();
    let (status, _) = call(&app, Method::POST, "/v1/accounts/refresh", None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn missing_api_key_is_unauthorized() {
    let (app, _store) = app();
    let request = Request::builder()
        .method(Method::GET)
        .uri("/v1/accounts")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
```

**注意依赖**:`oneshot` 需要 dev-dependencies 加 `tower`(features `util`)与 `http-body-util`:

```toml
[dev-dependencies]
tower = { version = "0.5", features = ["util"] }
http-body-util = "0.1"
```

- [ ] **Step 5: 跑测试**

Run: `cargo test --test accounts_contract`
Expected: 3 passed

- [ ] **Step 6: clippy + 提交**

```bash
cargo clippy --all-targets && git add src/server src/main.rs tests/accounts_contract.rs Cargo.toml Cargo.lock && git commit -m "Add /v1/accounts CRUD and refresh routes

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 10: main 装配刷新循环 + 文档收尾

**Files:**
- Modify: `src/main.rs`(启动刷新 + 定时循环;tokio 加 `time` feature)
- Modify: `Cargo.toml`(tokio features 加 `"time"`)
- Modify: `README.md`(账号管理一节)

**Interfaces:**
- Consumes: `refresh_all`/`REFRESH_INTERVAL`(Task 8)
- Produces: 完整运行时行为 —— 启动:开库 → 建表 → 导入 → 立即后台刷新 → 每 6 小时刷新。

- [ ] **Step 1: Cargo.toml** tokio features 加 `"time"`:

```toml
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal", "net", "time"] }
```

- [ ] **Step 2: main.rs**(state 构造后、listener 之前插入)

```rust
    // 启动立即刷新一遍账号额度,之后每 6 小时一遍;失败只记日志。
    {
        let accounts = accounts.clone();
        let client = client.clone();
        tokio::spawn(async move {
            loop {
                raycast2api::account::refresh::refresh_all(&accounts, &client).await;
                tokio::time::sleep(raycast2api::account::refresh::REFRESH_INTERVAL).await;
            }
        });
    }
```

同时把 Task 9 里的 `client.clone()` 依赖理顺:`AppState::new(settings.clone(), client.clone(), accounts)` 保持,spawn 块用 `client.clone()`(克隆的是 `RaycastClient`,内部全是 Arc,廉价)。

- [ ] **Step 3: README.md** 在功能列表加一行,并在接口表加:

```markdown
- **多账号管理**：SQLite 账号池，启动从 `account.json` 导入（email 去重、token 变化自动更新），请求轮询分摊额度；401 / 额度耗尽自动摘除，额度恢复自动上线；每 6 小时后台刷新额度。
```

```markdown
| `GET /v1/accounts` | 账号列表（token 掩码） | — |
| `POST /v1/accounts` | 新增/更新账号（按 email upsert） | — |
| `POST /v1/accounts/refresh` | 手动触发额度刷新（202，后台执行） | — |
| `DELETE /v1/accounts/{email}` | 删除账号 | — |
```

并在配置说明中:删除 `raycast_bearer_token` 行,加 `accounts_db`(默认 `accounts.db`)。

- [ ] **Step 4: 全量验证**

Run: `cargo test && cargo clippy --all-targets && cargo build`
Expected: 全部测试通过、零警告、构建成功

- [ ] **Step 5: 手工冒烟**(可选但推荐)

```bash
DEBUG_DUMP_DIR=./debug-dump cargo run
# 另一终端:
curl -s -H "Authorization: Bearer <api_key>" http://localhost:8787/v1/accounts | jq
```
Expected: 返回 account.json 导入的账号列表(token 掩码、available=true);网关日志出现 `account import from 'account.json'` 与 `account refresh` 两条 info。

- [ ] **Step 6: 提交**

```bash
git add src/main.rs Cargo.toml Cargo.lock README.md && git commit -m "Wire account refresh loop and document account management

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## 自检记录(计划完成后)

- **Spec 覆盖**:导入三态(T3/T4)、轮询(T3/T5)、401 摘除(T5)、额度刷新+恢复(T7/T8)、CRUD+refresh 端点(T9)、config 清理与 accounts_db(T6)、6 小时循环(T10)、并发隔离(无需代码,设计文档已论证)—— 全覆盖。
- **占位符**:无 TBD/TODO;所有代码步骤给出完整代码。
- **类型一致性**:`AccountStore::new(Db)`、`next_available() -> Result<AccountRow, GatewayError>`、`UpsertOutcome`、`AccountInfo::from((RaycastMe, RaycastCredits))`、`AppState::new(settings, client, accounts)`、`RaycastClient::new(http, settings, accounts)` 在各任务间一致。
