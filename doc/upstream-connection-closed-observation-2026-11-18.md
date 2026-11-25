# 上游 "connection closed before message completed" 观察手册

记录日期：2026-11-18（Asia/Shanghai）

范围：线上反复出现的上游传输错误。本文说明这个错误的含义、已经加上的诊断日志、观察时怎么读数据，以及不同结果对应的下一步。本文不含修复，修复要等观察结果。

## 1. 现象

线上多次出现：

~~~text
upstream request error: error sending request for url (https://backend.raycast.com/api/v1/ai/chat_completions): client error (SendRequest): connection closed before message completed
~~~

已知条件：

* 当时线上直连和代理出口都出现过该错误；当前出口由每个账号的设置决定。
* 提交 `7907115` 已把连接池空闲回收时间从 reqwest 默认的 90s 改为 20s（`UPSTREAM_POOL_IDLE_TIMEOUT`），出错的线上构建是否包含该提交需要先确认。

## 2. 错误的含义

这是 hyper 的 `IncompleteMessage`：请求已经写进连接，在收到完整响应头之前，对端关闭了连接。

hyper-util 0.1.20 只会自动重试"请求尚未写出"的情况（错误类型 `Canceled`，且连接来自连接池）。本错误的类型是 `SendRequest`，说明请求可能已被上游处理，hyper 无法判断，所以不重试。reqwest 0.12.28 的默认重试策略只处理 HTTP/2 NACK；2026-11-18 观察时的构建未启用 h2、走 HTTP/1.1，同样不会重试。后续版本已为 chat 启用 h2 协商，got 管道仍锁定 HTTP/1.1，见 TLS 指纹文档。

本地假服务复现结果（独立程序，不在仓库内）：

| 场景 | 结果 |
| --- | --- |
| 服务端读完请求后直接关闭连接 | 报本错误（全新连接也会报） |
| 第一个请求正常，连接入池，第二个请求复用该连接时被对端关闭 | 报本错误 |
| 对端在请求发出之前就关闭了空闲连接 | 不报错，hyper 自动换新连接 |

## 3. 网关当前的处理

| 位置 | 行为 |
| --- | --- |
| `src/raycast/transport.rs` `send()` | 按已选账号的设置使用直连或指定的单个代理；仅账号勾选「代理失败回退直连」时可在代理失败后直连。未选号则拒绝请求 |
| `src/account/dispatcher.rs` `send_chat()` | 该类错误在账号允许的回退用尽后直接返回，不自动换号 |
| `src/error.rs` `From<reqwest::Error>` | 连接、传输、解码类错误映射成 `Upstream { status: 502 }`，客户端收到 HTTP 502（`bad_gateway`）。只有请求构造失败（`is_builder()`）仍是 `Internal`（500）。2026-11-18 已从全部 500 改为此行为 |

## 4. 两类候选成因

| 成因 | 描述 | 特征 |
| --- | --- | --- |
| a. 复用了被悄悄关闭的空闲连接 | NAT、负载均衡或边缘设备提前回收连接，对端关闭恰好落在"取出连接、写请求"的窗口内 | 失败很快（几十毫秒），新连接重试通常能成功 |
| b. 上游收下请求后主动断开 | 后端异常、边缘掐连接、风控或 WAF 丢弃 | 失败较慢（几百毫秒到数秒），可能与账号、模型或请求体有关，重试未必有用 |

若账号勾选回退，同一请求在代理和直连上均失败，对 a 不利：回退意味着使用另一连接，若是 a 通常能成功。这是推断，需要下面的数据验证。仅观察到不同账号的直连、代理请求都失败，不能证明同一请求已经执行过回退。

## 5. 已加的诊断日志

位置：`src/raycast/transport.rs` 的 `send()`。每次通道尝试失败时输出一条 `warn`，前缀 `→ upstream send failed`，覆盖传输错误和响应头超时两条路径。

| 字段 | 含义 |
| --- | --- |
| `channel` | `direct` 或该账号指定的代理 ID，可在代理管理列表中查到 HOST/PORT |
| `account` | 账号邮箱（仅聊天请求有，`get_json` 的 GET 请求为 `None`） |
| `elapsed_ms` | 从发起请求到失败的耗时 |
| `body_bytes` | 请求体字节数（仅内存字节体有值，GET 请求为 `None`） |
| `is_connect` | `true` 表示连不上，`false` 表示连上后被断 |
| 消息正文 | 完整错误链，包含 hyper 的真实错误类型 |

说明：

* 账号勾选回退时，同一个请求可能在其指定代理和 `direct` 上各出一条；未勾选时不会发送直连请求。
* 该改动需要部署后才生效，部署前的历史失败没有这些字段。
* 本次改动未提交，也未部署。

提取日志：

~~~bash
grep "upstream send failed" <日志文件>
~~~

## 6. 用统计库补充历史数据

`statistics` 表对每次通道尝试记一行，`error_message` 保存错误文本，`started_at` 和 `finished_at` 为整秒时间戳。`request_logs` 表按同一个 `request_id` 保存发给上游的请求体。数据库文件是配置里 `accounts_db` 指向的 SQLite。

查询前请只读打开，避免干扰线上写入：

~~~bash
sqlite3 -readonly <accounts_db 路径>
~~~

下面的窗口为最近 3 天，按需调整 `86400 * 3`。

### 6.1 失败是否集中在某些账号

~~~sql
SELECT account_email,
       COUNT(*) AS total,
       SUM(COALESCE(error_message, '') LIKE '%connection closed before message completed%') AS closed,
       ROUND(100.0 * SUM(COALESCE(error_message, '') LIKE '%connection closed before message completed%') / COUNT(*), 2) AS pct
FROM statistics
WHERE requested_at > strftime('%s', 'now') - 86400 * 3
GROUP BY account_email
ORDER BY closed DESC;
~~~

看 `pct` 而不是 `closed`：流量大的账号绝对数必然多。

### 6.2 失败是否集中在某些模型

~~~sql
SELECT upstream_model,
       COUNT(*) AS total,
       SUM(COALESCE(error_message, '') LIKE '%connection closed before message completed%') AS closed
FROM statistics
WHERE requested_at > strftime('%s', 'now') - 86400 * 3
GROUP BY upstream_model
ORDER BY closed DESC;
~~~

### 6.3 失败的耗时分布

~~~sql
SELECT finished_at - started_at AS secs, COUNT(*) AS n
FROM statistics
WHERE error_message LIKE '%connection closed before message completed%'
  AND requested_at > strftime('%s', 'now') - 86400 * 3
GROUP BY 1
ORDER BY 1;
~~~

秒级精度，只够区分"接近 0 秒"和"几秒"。更精细的耗时看第 5 节日志的 `elapsed_ms`。

### 6.4 失败请求的请求体大小

~~~sql
SELECT COALESCE(s.error_message LIKE '%connection closed before message completed%', 0) AS closed,
       COUNT(*) AS n,
       ROUND(AVG(LENGTH(r.upstream_json))) AS avg_len,
       MAX(LENGTH(r.upstream_json)) AS max_len
FROM statistics s
JOIN request_logs r ON r.request_id = s.request_id
WHERE s.requested_at > strftime('%s', 'now') - 86400 * 3
GROUP BY 1;
~~~

`request_logs` 有保留期，超过保留期的请求体已被清理，这条只能看到仍保留的记录。`upstream_json` 是网关记录的请求体副本，长度与实际发出的字节数只是近似。

以上查询已在内存库里用仓库的真实表结构验证过语法，未在线上数据上运行。

## 7. 结果怎么读，下一步怎么走

| 观察到 | 倾向 | 下一步 |
| --- | --- | --- |
| `elapsed_ms` 多在几十毫秒内，散布在所有账号和请求体大小上 | a：连接层问题 | 在 `send()` 里对该错误在同一通道用新连接重试一次；必要时进一步缩短空闲回收时间或评估 HTTP/2 |
| `elapsed_ms` 为几百毫秒到数秒 | b：上游主动断开 | 重试收益有限，先看是否集中在账号、模型或请求体上 |
| 集中在个别账号（6.1 的 `pct` 明显偏高） | 账号被上游限流或风控后掐连接 | 评估把这类错误计入账号健康（冷却或摘号），而不是重试 |
| 失败请求体明显更大（6.4） | 请求大小或内容触发 | 定位上游的体积限制，重试无效 |
| 同一请求在 `direct` 和代理上各出一条 | 两条通道都被掐，更像上游行为 | 同上，按 b 处理 |

同出口重试的风险：请求可能已被上游处理，极少数情况下会重复计费。账号勾选代理失败回退直连时，普通请求允许在传输失败后直连重试；OAuth 续期仅在代理连接建立失败时回退，不重放可能已发出的请求（`oauth_token`）。

状态码已从 500 改为 502（见第 3 节）。副作用：`quota_refresh::fetch_info` 里查 `/me/trial_status` 时，网络错误现在会命中 `Upstream { status, .. }` 分支，debug 日志写成 "returned 502 (非试用号属正常)"，不再是 "failed: ..."。只影响 debug 日志文案，不影响账号可用性判定（`is_account_dead_upstream` 只看 402 和特定文案）。

## 8. 状态

| 项 | 状态 |
| --- | --- |
| 诊断日志（`send()`） | 已实现，`cargo check` 通过，4 个 transport 相关 lib 测试通过；未提交、未部署 |
| 500 改 502（`From<reqwest::Error>`） | 已实现，全量 `cargo test` 通过（含新增的 502 与 builder 错误两条测试）；未提交、未部署 |
| 根因 | 未确认 |
| 同通道重试 | 未实现，等观察结果 |
| 线上数据 | 未查询 |
