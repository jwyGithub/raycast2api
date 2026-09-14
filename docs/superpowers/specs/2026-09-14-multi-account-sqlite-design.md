# 多账号管理(SQLite)设计

日期:2026-09-14
状态:已批准

## 目标

网关支持多个 Raycast 账号:SQLite 持久化、启动时从 `account.json` 导入、
上游请求轮询使用、HTTP CRUD 管理、额度/订阅信息自动刷新、
401 与额度耗尽自动摘除。

## 需求(已确认)

1. 启动时将 `account.json` 的账号导入 SQLite 账号表;唯一标识为
   `user.email`;已存在且 `access_token`/`refresh_token` 不同则更新。
2. 上游请求按轮询(round-robin)使用账号,分摊额度。
3. 提供完整 CRUD HTTP 接口(挂 API key 层后)。
4. 不做 token 刷新:账号 401 时标记不可用并跳过,靠重新导入 / POST 更新恢复。
5. 推理请求只用 SQLite 账号;`config.json` 的 `raycast_bearer_token`
   不再参与推理(字段保留兼容,被忽略)。
6. 账号额度与订阅信息(参考 `docs/superpowers/specs/_account_info.mjs`
   逆向的两个上游接口):启动时后台刷新一遍,之后每 6 小时定时刷新;
   额度耗尽的账号自动摘除,下期发放(`next_credits_at`)后经刷新自动恢复。

## 模块结构(遵循现有分层)

```
src/db.rs               新顶层模块:共享 SQLite 基础设施,不含任何业务表知识
                        Db = Arc<Mutex<Connection>>(bundled rusqlite)
                        open(path)、conn() 访问器、execute_batch 辅助
src/account/            新顶层模块:账号域,建在 Db 之上
  mod.rs      模块文档 + re-export
  store.rs    AccountStore:包装 Db;accounts 建表 DDL、CRUD、轮询选号、
              摘除/恢复、额度信息更新
  import.rs   account.json 反序列化与启动导入(upsert 三态语义)
  refresh.rs  额度刷新:遍历账号调 /me + /ai/credits,结果入库;
              401 或额度耗尽 → 摘除,有余额 → 恢复
  wire.rs     对外 JSON 形状:AccountRecord(导入/POST 请求体)、
              AccountInfo(me/credits 响应中要存的字段的瘦模型)、
              AccountView(GET 列表响应,token 掩码)
src/server/handlers/accounts.rs   路由 handler,薄壳,只做提取与渲染
```

依赖方向:`server` → `account` → `db`;`account::refresh` 调用
`raycast::client` 的查询接口(见下),`raycast::client` 构造时注入
`Arc<AccountStore>`(与现有 `Settings` 注入方式一致,`refresh` 由
`main` 装配,避免 `raycast` ↔ `account` 循环)。

**扩展方式**:后续模块(用量统计、会话等)同样建在 `db` 之上 —— 各自的
`store` 持有 `Db`、自己执行 `CREATE TABLE IF NOT EXISTS`。`db.rs` 不出现
任何具体表;新增使用方不需要改动 `db.rs`,只在 `main` 装配时多一行。
SQLite 路径、连接策略(bundled、WAL、busy timeout)集中在一处。

技术选型:`rusqlite`(feature = "bundled"),`Arc<Mutex<Connection>>` 包
同步连接;轮询计数器 `AtomicUsize` 存于 `AccountStore`。网关流量下锁竞争
可忽略;换异步驱动(sqlx 等)时只动 `db.rs` 一个文件,各业务 store 的
函数签名从 `&Db` 出发,不受影响。

## 表结构

```sql
CREATE TABLE IF NOT EXISTS accounts (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    email               TEXT NOT NULL UNIQUE,
    tag                 TEXT,
    access_token        TEXT NOT NULL,
    refresh_token       TEXT,
    obtained_at         TEXT,
    user_json           TEXT,   -- user 对象原文,保留订阅状态等
    available           INTEGER NOT NULL DEFAULT 1,
    unavailable_reason  TEXT,
    -- 额度/订阅信息(refresh 从 /me + /ai/credits 拉取)
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
```

## 核心行为

### 启动导入(import.rs)

读 `account.json`(相对 CWD,同 config.json 约定),逐条按 email upsert:

- 不存在 → 插入;
- 存在且 (access_token, refresh_token) 任一不同 → 更新全部业务字段,
  并重置 `available = 1`(新 token 可能已恢复);
- 存在且 token 完全相同 → 跳过(不动 available 状态)。

`user.email` 缺失的条目跳过并 `warn!`。文件不存在 → 静默跳过。
导入结果记一条 `info!`(新增 n / 更新 n / 跳过 n)。

### 轮询(store.rs)

`next_available()` → 从 `counter % N` 起循环扫描可用账号,命中即自增
counter 返回;全不可用 → `GatewayError`(503,提示无可用账号,建议
POST /v1/accounts 更新 token)。counter 跨账号增删保持单调,无需重置。

`mark_unavailable(email, reason)` → 置 `available = 0` + reason。
摘除来源两处:
- `send_chat` 的 `ensure_success` 返回 `Upstream { status: 401 }` 时,
  由 client 调用(reason: "token 失效 (401)");
- `refresh` 发现额度耗尽(reason: "额度耗尽")或查询 401。

### 额度刷新(account/refresh.rs + raycast/client.rs)

数据源(逆向自 `_account_info.mjs`,均只需 Bearer,无需签名):
- `GET /api/v1/me` → subscription(tier/status/trial_days_left/
  current_period_end)
- `GET /api/v1/ai/credits` → remaining_balance_credits /
  total_balance_credits / remaining_balance_usd / next_credits_at

`raycast::client` 新增 `fetch_me(token)`、`fetch_credits(token)`
(复用 `base_headers` 的无签名分支 + `ensure_success`)。

`refresh_all(store, client)`:顺序遍历库中账号,每账号并发拉两个接口,
结果经 `store.update_info(email, info)` 入库并刷新 `info_fetched_at`;
随后按结果调整可用性 —— 401 或 `remaining_credits <= 0` → 摘除,
有余额且原因为"额度耗尽"的 → 恢复(`available = 1`)。单个账号失败
不影响其余(warn 日志)。

调度(`main` 装配):启动导入完成后 spawn 一个循环 task —— 立即刷新
一遍,之后每 6 小时(`const REFRESH_INTERVAL`)再刷。账号数量小,
顺序遍历即可,不引入并发池。

### 请求侧(client.rs)

`RaycastClient` 新增 `accounts: Arc<AccountStore>` 字段;
`base_headers`/`send_chat`/`fetch_models` 不再读
`settings.raycast_bearer_token`,改为 `store.next_available()?` 取
token。`device_id`、`signature_secret` 仍来自全局 Settings。

### CRUD 接口(server/handlers/accounts.rs)

| 路由 | 行为 |
|---|---|
| `GET /v1/accounts` | 列表;token 掩码为前 8 位 + `...`;含可用状态/原因与额度字段 |
| `POST /v1/accounts` | 请求体 = account.json 单条形状;按 email upsert(与导入同一语义),返回导入结果(插入/更新/跳过) |
| `POST /v1/accounts/refresh` | 手动触发一次全量额度刷新(后台执行,立即返回) |
| `DELETE /v1/accounts/{email}` | 删除;不存在 → 404 |

路由注册进现有 `api`(在 `require_api_key` 层内)。

## 配置变更(config.rs)

- `ConfigFile`/`Settings` 新增可选 `accounts_db`,默认 `"accounts.db"`。
- `Settings::raycast_bearer_token` 移除(推理不再使用);`ConfigFile` 中
  该字段一并移除 —— serde 对多余字段宽容,旧 config.json 无需修改即可解析。
- 启动校验:`RAYCAST_BEARER_TOKEN` 不再是必填;`RAYCAST_DEVICE_ID` 仍必填。

## 错误语义

- 库中无可用账号 → `GatewayError`(503,"no available Raycast account"),
  经现有错误渲染通道返回各协议错误信封。
- account.json 存在但格式非法 → 启动失败并报明确错误(而非静默)。

## 测试(内存库,不落盘)

1. `store`:插入 / 更新(token 变化)/ 跳过(token 相同)三态;
   email 唯一约束;`update_info` 摘除与恢复。
2. 轮询:顺序循环;删除中间账号后顺序仍正确;全不可用时报错;
   `mark_unavailable` 后被跳过。
3. `import`:正常导入、email 缺失跳过、文件缺失静默。
4. `refresh`:对 mock 的 me/credits 响应入库、401 摘除、零余额摘除、
   有余额恢复。
5. handler 契约:`GET` 掩码、`POST` upsert、`DELETE` 404。
