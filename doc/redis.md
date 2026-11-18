# Redis 接入与使用范围

本文基于 2026-11-16 的代码。已实现 Dashboard Session、上下文压缩摘要缓存和实测输入 Token 缓存；其余场景仅为分析。

## 已实现的 Session

连接由 `src/storage/redis_store.rs::connect_from_env` 创建，读取 Compose 中的 `REDIS_HOST`、`REDIS_PORT`、`REDIS_USERNAME`、`REDIS_PASSWORD`、`REDIS_DB`。异步 ConnectionManager 复用连接并自动重连；连接和响应超时均为 3 秒。配置了控制台密码却缺少 Redis，或配置的 Redis 无法连接，启动时直接报错。

`src/server/session.rs` 实现会话生成、签名校验、Redis 检查和注销：

* 浏览器 Cookie 为安全随机 ID 和 HMAC-SHA256 签名，签名绑定 `dashboard_session_secret` 及当前控制台密码；随机源失败时拒绝登录。
* Key 为 `<prefix>:dashboard:session:<SHA256(cookie token)>`，值固定为 `1`，使用 `SET ... EX 86400` 原子设置固定 24 小时有效期；不存密码、API Key 或可直接使用的 Cookie Token。
* 每次 Cookie 鉴权先检查签名，再检查 Redis Key；过期、删除、密钥变化、密码变化均使会话失效。密码和密钥配置需重启应用才生效，滚动部署时应保持所有实例配置一致。
* 退出登录执行 `DEL` 并清除 Cookie；同一浏览器再次登录会撤销携带的旧会话。重新登录并发请求可能留下不可再由浏览器访问的记录，均由固定 TTL 清理。
* 页面刷新调用 `POST /v1/session` 恢复；Cookie 请求必须带 `X-Dashboard-Request: 1`，配合 `SameSite=Strict` 和不开放跨域凭据的 CORS 防止跨站表单调用。
* 管理接口接受 Session 或配置的 API Key；推理和模型接口维持现有 API Key 规则。未配置 API Key 时管理接口仍须 Session。导出账号仍保留“必须配置 API Key”的既有要求。
* Redis 运行中不可用时，Session 请求返回 503，API Key 鉴权不依赖 Redis。Redis 重启后是否保留会话取决于外部 Redis 的持久化策略；记录丢失只需重新登录。

Session 密钥至少 32 字节，建议使用 `openssl rand -hex 32` 生成。通过 HTTPS 反向代理访问时，代理必须覆盖 `X-Forwarded-Proto: https`，Cookie 才会附加 `Secure`。当前连接配置使用 TCP，未增加 Redis TLS 或 Redis Cluster 支持。

## 已实现的压缩缓存

`src/server/compact.rs` 复用 `AppState` 的 Redis 连接和 `REDIS_KEY_PREFIX`，无需增加配置。未配置 Redis 时仍使用本地缓存。

| 缓存 | Redis Key | 值 | TTL 与本地容量 |
| --- | --- | --- | --- |
| `SummaryCache` | `<prefix>:compact:v1:summary:<SHA256>` | 带固定标记的摘要正文 | 30 分钟；本地最多 32 个模型，每模型 128 条 |
| `UsageCache` | `<prefix>:compact:v1:usage:<SHA256>` | 上游实测输入 Token 数 | 30 分钟；本地最多 1024 条 |

* 两类 Key 均使用结构化 JSON 输入的 SHA-256，包含模型、system、工具定义、工具选择、推理配置，以及每条消息的角色、文本、工具调用和工具结果元数据。消息和字段边界参与哈希，避免原摘要 FNV 拼接产生的歧义。键格式或压缩规则变动时需要升级 `v1`。
* 客户端追加历史后，选择本地与 Redis 中覆盖最多消息的精确前缀。本地已有较短前缀时，仍查询 Redis 中更长的候选；完整本地命中时无需远程读取。每类缓存一次 `MGET` 批量查询候选，无逐条消息的网络往返。
* 完整摘要命中时直接复用；部分命中时仅总结剩余历史，并保留原消息偏移和当前用户任务。用量命中时按“实测前缀 Token 数 + 新增消息字节估算”计算；压缩后的请求用量不会套用到原始完整历史。上游不重放的 thinking 文本和 stream 标志不参与哈希。
* 成功记录先写本地，再以 `SET ... EX 1800` 写 Redis；零用量、空历史和失败/空摘要不写入。读取不续期，也不将远程值回填成新的 30 分钟本地缓存。重新生成同一键的有效结果会重新开始该键的 TTL。
* 缓存每次 Redis 读写最多等待 100 毫秒；错误或超时记录警告，读取按未命中处理，写入保留本地结果。网络等待不持有本地缓存锁。用量写入在推理结束事件前最多增加一次 100 毫秒等待；压缩规划最多有两次独立缓存读取。无效数字、零值、非法 UTF-8、错误 Redis 类型和缺少标记的摘要均不参与命中。

摘要包含对话的衍生内容，Redis 不再只有 Session 标记和哈希。应限制 Redis 访问权限，并按对话数据的敏感程度管理 Redis 备份；用量缓存只存哈希与计数。Redis 缓存按 TTL 清理，没有额外条数上限，应按请求量规划内存；淘汰或丢失缓存只影响复用，丢失 Session 需要重新登录。生产 Redis 的淘汰策略应同时考虑两类数据。

## 其他逻辑的适配建议

| 优先级 | 当前逻辑与位置 | Redis 用法 | 边界与故障策略 |
| --- | --- | --- | --- |
| 中 | `src/server/state.rs::models`：模型列表 TTL 5 分钟，失败退避 30 秒，失败可读旧列表 | 共享模型快照和刷新时间，降低多个实例重复拉取；短租约抑制并发刷新 | 不能简单用 5 分钟过期删除唯一副本，否则丢失现有 stale-on-error 行为；需分别管理新鲜期与旧副本保留期，Redis 故障可回退本地缓存 |
| 多实例前必做 | `src/account/token_renewal.rs::auto_renew` 与 `AccountStore::renewal_lock`：目前仅进程内串行续期 | 每账号跨进程租约；成功后以持久化版本或旧 token 为条件写回新凭据 | refresh_token 会轮换，必须处理租约续期、超时、不确定响应、旧持有者迟到和原子写回；简单加 `SET NX EX` 不足以保证安全。锁内必须读到共享账号库中的最新凭据 |
| 多实例时 | `src/main.rs` 的启动及每 6 小时额度刷新 | 任务租约或主实例选举，防止多实例重复轮询所有账号 | 手动刷新与续期路径仍需遵守账号级互斥；不能只锁定定时任务 |
| 有多实例或限流需求时 | `AccountStore::last_id`、`cooldown_until`，以及进程内 64 路 `inference_slots` | 全局轮询游标、共享账号冷却、每账号/API Key 速率计数、全局在途租约 | 当前冷却已在 SQLite 持久化，单实例无需复制两份真值；全局并发必须处理 SSE 长连接、断开和进程崩溃。保留本地 Semaphore 保护进程资源 |
| 中 | `POST /v1/login` 目前没有失败次数限制 | Redis 原子计数与 TTL，限制密码尝试；可同时按可信来源和全局窗口限制 | 需先定义阈值和可信代理链，不能直接信任任意 X-Forwarded-For，也应避免攻击者轻易锁住所有管理员 |
| 低 | `src/statistics/store.rs::list` 的筛选、分页和总数 | 对已证实较慢的聚合结果做短 TTL 缓存 | 当前页面主要查明细，缓存命中率未测量；原始统计保留数据库，先测 SQL 和索引，再决定是否引入聚合缓存 |

后续视部署规模接模型缓存和跨实例账号租约。缓存失败应降级，认证与账号续期协调失败应停止依赖该状态的操作。

## 当前无需迁移的内容

* 账号原始凭据、请求日志全文和调用统计：继续使用 SQLite。Redis 可作缓存或协调层，不替代这些持久化记录。
* 请求内的 SSE Decoder、重试输出缓冲、请求通道登记与任务局部上下文：属于当前连接和当前进程的短期状态，远程存储会增加延迟和清理成本。
* `BackendHealth`：上游可达性可能因实例网络或代理不同，保留实例自身健康状态；共享模型缓存与健康探测分别管理。
* 完整对话历史：三个推理协议目前携带历史消息，`thread_id` 可确定性派生。没有实现服务端对话 ID 续接，因此移除了无消费者的 `REDIS_CONV_TTL=72h`；Session TTL 与对话缓存无关。

## 多实例部署限制

共享 Redis 已能让 Dashboard Session 跨应用实例恢复，前提是 Redis 数据库、Key 前缀、控制台密码与 Session 密钥一致。这不等于整个网关已支持多实例共用账号：

* 每实例独立的 SQLite 不会因共享 Redis 自动同步账号、最新 token 或冷却状态。
* 当前续期锁是进程内锁，不能防止跨实例重复消费会轮换的 refresh_token。
* `src/main.rs` 启动时调用 `StatisticsStore::interrupt_unfinished`，将全部 `in_progress` 记录标为中断；多个实例共用统计库时会误伤其他实例仍在运行的记录，需要实例归属和存活租约。
* 多主机共享 SQLite 文件还受 WAL 与文件锁语义限制，不能把网络盘挂载当作可直接使用的共享数据库方案。

在完善共享持久化、续期租约和统计恢复规则前，仍按单网关实例部署。

## 验证

常规测试不依赖已有 Redis。另有显式集成测试，在本机临时 Unix Socket 启动独立 Redis，使用结束后终止进程并删除 Socket，不访问已有 Redis 数据：

```bash
cargo test --locked
cargo test --locked --lib server::compact::redis_tests -- --ignored
cargo test --locked --test dashboard_contract redis_session_lifecycle_and_authentication_boundaries -- --ignored
node --check src/page/views.js
git diff --check
```

验收包括固定 TTL、有效 Session 调用管理接口、刷新恢复所需的 Session 校验、跨应用实例共享、注销、过期、篡改拒绝、修改密码/密钥失效、Redis 故障返回 503、API Key 在 Redis 故障时仍可调用管理接口，以及 Session 不能替代配置的推理 API Key。

缓存测试另覆盖全新应用状态复用、命名空间/提示词隔离、最长分支前缀、增量压缩、TTL 与读取不续期、无效缓存值、Redis 暂停时的等待上限和锁释放、连接丢失时的本地降级，以及 Redis 不可用时仍能完成压缩。摘要生成使用测试闭包，不调用真实 Raycast。

参考：[redis-rs 异步客户端](https://github.com/redis-rs/redis-rs)、[Redis Session 存储](https://redis.io/docs/latest/develop/use-cases/session-store/)、[Redis SET 命令](https://redis.io/docs/latest/commands/set/)。
