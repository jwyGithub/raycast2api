# 请求日志入库(request_logs)设计

日期:2026-09-14
状态:已批准

## 目标

把现有 `DEBUG_DUMP_DIR` 文件落盘的三件套(客户端请求体、发给上游的请求体、
上游 SSE 响应)改为写入 SQLite,按时间组织,保留 3 天,超期自动清理,
并提供 HTTP 查询接口。

## 需求(已确认)

1. 采集默认开启(无环境变量门控)。
2. 查看走 HTTP 接口(列表 + 详情),挂 API key 层内。
3. 保留 3 天(`created_at` 早于 `now - 259200` 秒即清理)。
4. 体积不做单条截断、不做总量上限,纯靠时间窗口控制。
5. 文件落盘逻辑(`debug::dump` / `dump_enabled` / `DEBUG_DUMP_DIR`)移除,
   被库存储替代;`debug.rs` 保留 `REQUEST_ID` task-local 与 `body_to_value`。

## 模块结构(遵循现有分层)

```
src/log/                新顶层模块:请求日志域,建在共享 db 上
  mod.rs      模块文档 + re-export
  store.rs    RequestLogStore:建表 DDL、insert_request / update_upstream /
              update_response、list / detail、cleanup
  wire.rs     LogEntry(列表项,不含 body)、LogDetail(详情,三体全文)
src/server/handlers/logs.rs   两个路由 handler,薄壳
```

依赖方向:`server` → `log` → `db`;`raycast::client` 与
`raycast::stream` 消费 `Arc<RequestLogStore>`(与 `AccountStore` 注入方式
一致)。**日志写入是旁路:任何失败只 `warn!`,不影响请求处理。**

## 表结构

```sql
CREATE TABLE IF NOT EXISTS request_logs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id    TEXT NOT NULL UNIQUE,   -- REQUEST_ID task-local ULID
    created_at    INTEGER NOT NULL,       -- epoch 秒
    method        TEXT,
    path          TEXT,
    inbound_json  TEXT,                   -- 客户端请求体(pretty JSON)
    upstream_json TEXT,                   -- 发给 Raycast 的请求体(pretty JSON)
    response_text TEXT                    -- 上游 SSE 响应,NDJSON 原文
);
CREATE INDEX IF NOT EXISTS idx_request_logs_created ON request_logs(created_at);
```

一行一请求,三列渐进补写:middleware 插入行,client 补上游体,
流结束时补响应体。`request_id` 唯一键保证三处落在同一行。

## 核心行为

### 采集点(替换文件 dump)

| 采集点 | 动作 |
|---|---|
| `server/middleware.rs::debug_capture` | `log.insert_request(request_id, method, path, inbound_body)`(INSERT,若行已存在则跳过——请求重放不会撞键) |
| `raycast/client.rs::send_chat` | `log.update_upstream(request_id, upstream_body)`(发送前) |
| `raycast/stream.rs::canonical_events` | `log.update_response(request_id, payloads.join("\n"))`(解析出 `Finish` 事件的当下,同现有 ndjson 时机) |

注入方式:
- `AppState` 加 `pub log: Arc<RequestLogStore>`(middleware 与 handler 使用);
- `RaycastClient` 加 `log: Arc<RequestLogStore>` 字段(send_chat 使用);
- `canonical_events` 增加参数 `log: Arc<RequestLogStore>`,`pipeline::run`
  唯一调用方,从 `state.log` 传入;request_id 仍在函数体开头于
  REQUEST_ID scope 内抓取(`stream!` 为 `async move`,已验证)。

### 清理(3 天)

- 启动时执行一次(建表后);
- **每次 `insert_request` 顺带执行**(created_at 有索引,无过期行时
  近零成本;不需要定时任务):

```sql
DELETE FROM request_logs WHERE created_at < ?1  -- now - 259200
```

### 查询接口

| 路由 | 行为 |
|---|---|
| `GET /v1/logs?limit=50&before_id=<id>` | 列表,按 id 降序(即时间降序);每项含 request_id / created_at / method / path / 三体字节大小;`limit` 上限 200,`before_id` 用于翻页 |
| `GET /v1/logs/{request_id}` | 详情,三体全文;不存在 → 404 |

路由挂 `require_api_key` 层内,与 `/v1/accounts` 同段。

## 移除项

- `src/debug.rs`:`dump()`、`dump_enabled()`、`dump_dir()` 删除
  (`DEBUG_DUMP_DIR` 不再有效);`REQUEST_ID`、`current_request_id`、
  `body_to_value` 保留。
- 三个采集点的文件写代码全部替换(见上表)。
- README 中 `DEBUG_DUMP_DIR` 相关段落改为请求日志接口说明。

## 错误语义

- 采集写入失败(INSERT/UPDATE 出错)→ `warn!`,请求照常处理。
- 列表参数非法(limit 非数字等)→ 400。
- 详情无此 request_id → 404。

## 测试(内存库)

1. `store`:insert 建行;update_upstream / update_response 补列且
   不覆盖已有列;重复 insert 同 request_id 不报错;cleanup 删除
   3 天前的行、保留新行;list 降序 + before_id 翻页;detail 取全三体。
2. handler 契约:列表不含 body 字段;详情三体齐全;不存在 404;
   未带 API key → 401。
