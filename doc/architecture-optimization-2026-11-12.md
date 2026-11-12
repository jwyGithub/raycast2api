# Raycast2API 项目架构与优化记录

记录日期：2026-11-12（Asia/Shanghai）

范围：Rust 网关、账号管理、模型缓存、日志和部署构建。本文基于当前工作区，完成标记代表实现与离线验证，不代表生产发布完成。

## 1. 当前架构

三个协议的 inbound 将客户端请求转换成 CanonicalRequest。共享 pipeline 负责模型解析、构造 Raycast 请求、账号调度、发送和事件流创建，outbound 按客户端协议渲染。Responses 压缩过程保留独立的进度编码，摘要调用已复用 pipeline。

~~~mermaid
flowchart LR
    A[客户端] --> B[鉴权与推理准入]
    B --> C[协议 inbound]
    C --> D[CanonicalRequest]
    D --> E[共享 pipeline]
    E --> F[模型缓存与解析]
    E --> G[账号调度与续期]
    G --> H[Raycast transport]
    H --> I[首包检查与 SSE 解码]
    I --> J[CanonicalEvent]
    J --> K[协议 outbound]
    K --> A
    E --> L[SQLite 日志]
~~~

| 模块 | 职责 |
| --- | --- |
| src/core/ | 统一请求、消息、工具、事件、响应；暂保留分页 DTO |
| src/protocol/ | Chat Completions、Messages、Responses 各自的 wire/inbound/outbound/error |
| src/raycast/builder.rs | CanonicalRequest 到上游请求的纯转换 |
| src/raycast/client.rs | 签名、账号轮换、401 恢复、账号级拒绝处理 |
| src/raycast/transport.rs | 通道顺序、网络回退、响应头超时、OAuth 单次发送策略 |
| src/raycast/peek.rs | 有界读取完整首 payload，识别流内账号错误并保留剩余字节 |
| src/raycast/stream.rs | SSE 事件解析、日志捕获、断流重试和前缀检查 |
| src/raycast/models.rs | 唯一的模型纯匹配与默认规则 |
| src/account/token_renewal.rs | 账号串行续期、凭据重读、错误分类和落盘 |
| src/account/store.rs | 账号持久化、游标选择、续期锁注册 |
| src/account/quota_refresh.rs | 额度查询、试用到期与可用性决策 |
| src/account/service.rs | 手动恢复账号的应用编排；保留原有恢复策略 |
| src/log/context.rs | 生产请求关联 ID 与通道追踪，迁自 src/debug.rs |
| src/log/store.rs | 日志尽力写入、查询和清理 |
| src/server/pipeline.rs | 共享推理路径、聚合、真实推理健康状态更新 |
| src/server/state.rs | 共享资源、模型缓存与并发刷新协调 |
| src/server/compact.rs | Responses 上下文压缩与有容量限制的缓存 |
| src/db.rs | SQLite 连接和生产运行时同步阻塞区 |
| src/main.rs | 装配、监听、定时刷新与日志维护 |

## 2. 有序修复 TODO

编号对应架构审查，按此顺序开始修改，依赖项在后续阶段统一验证。

* [x] 01 单实例同账号续期串行化；锁内重读 token，复用其他请求已更新的凭据。
* [x] 02 保留凭据失效、临时上游失败和持久化失败分类；更新零行不能算落盘成功；网络失败不直接摘号。
* [x] 03 试用到期优先于非 Pro 的 credits 忽略规则，增加组合回归。
* [x] 04 续期后重试汇入共享 HTTP/SSE 分类，保留限额冷却、风控摘号和换号。
* [x] 05 跨 chunk 读取完整首 payload，心跳后继续扫描，保留剩余原始字节并设置等待和容量边界。
* [x] 06 增加连接、响应头、首包、流空闲和普通查询超时；限制推理 body、并发、日志与重试缓存。
* [x] 07 同步 Store 进入生产 Tokio 阻塞区；维护任务使用 spawn_blocking；日志列表用 SQL 计算字节数。
* [x] 08 模型缓存并发刷新串行化，失败使用旧值并退避 30 秒；无缓存时传播错误，不因网络故障改用默认模型。
* [x] 09 下一账号查询和游标更新保持在同一连接临界区，最多两次单行查询，避免加载全部凭据。
* [x] 10 GET 与聊天共享通道顺序和网络回退；OAuth 遵守首选通道但不重放可能已完成 rotation 的请求。
* [x] 11 删除无调用访问器、thinking 判断、协议 path 和旧事件入口；复用既有模型解析与摘要推理管线。
* [x] 12 文件/环境共用配置构造；仅文件不存在才回退，格式与读取错误终止启动。
* [x] 13 readiness 默认未就绪、检查可用账号；原子认领探测；真实推理 Finish/Error 更新状态。
* [x] 14 Docker 创建 store 目录和修改所有权移至 USER app 前。
* [x] 15 账号恢复移至 account/service，追踪移至 log/context，网络逻辑移至 raycast/transport。
* [x] 16 新增关键回归，执行离线测试、格式和严格 Clippy，更新架构文档入口。

## 3. 关键故障策略

### 续期

401 进入统一 auto_renew。按邮箱取得异步锁后重新读库；token 已变则复用，否则才使用 refresh token 换新。成功需要数据库写回且更新行数大于零。无 refresh token、invalid_grant 是凭据失效；网络、5xx 和响应解析失败保留为临时错误。

续期后的聊天继续执行首包分类，不再直接返回。OAuth 按配置选择首选 HTTP 通道，单次发送，不自动回退重放：请求失败不代表上游未完成 rotation。协调范围为同一个 AccountStore/进程。

### 模型与健康

模型缓存新鲜期 300 秒，刷新互斥，失败退避 30 秒，有旧值则继续使用，无值则报错。未知模型保留原默认规则；网络错误与未知模型分别处理。

/health 表示进程存活。/ready 需要可用账号，初始未就绪；失败时每 30 秒最多认领一次模型端点探测。真实推理完成标记成功，发送/事件错误标记失败。模型探测成功只证明模型端点可达，不保证下一次推理一定成功。

### 数据库与日志

SQLite 保持 WAL、busy_timeout=5000 和共享连接。生产二进制使用多线程 Tokio，Store 同步方法通过 block_in_place 让出异步调度容量；同步工具与 current-thread 测试直接执行。数据库连接仍串行，这不等于增加读写并行能力。

日志写入为同步尽力写入，失败只记日志，不影响推理结果；没有引入日志队列。维护任务使用 spawn_blocking，但仍与账号共用连接。日志列表查询元信息和 BLOB 长度，不再读取正文计算字节数。

## 4. 资源边界与兼容变化

| 项目 | 当前值或行为 |
| --- | --- |
| 推理并发 | 64；溢出 503；许可持有到响应体结束或断开 |
| 推理请求体 | 16 MiB；超限拒绝 |
| 入站日志正文 | 2 MiB；超限只记元信息，仍可推理 |
| 连接超时 | upstream_timeout_secs，默认 20 秒 |
| 普通 GET / OAuth | upstream_timeout_secs，默认 20 秒 |
| 普通通道响应头等待 | upstream_timeout_secs，默认 20 秒 |
| 首 SSE payload | upstream_timeout_secs（默认 20 秒），扫描容量 256 KiB；容量超限放回已读字节 |
| SSE 流空闲 | 等待下一 chunk upstream_timeout_secs（默认 20 秒）；无结束事件则按断流处理 |
| SSE 自动恢复 | 最多一次，已发送前缀必须一致 |
| 重试前缀 | 2 MiB 或 16,384 事件；超限后不再自动重放 |
| 响应日志 | 2 MiB 后附截断标记，不截断实际输出 |
| 摘要缓存 | 30 分钟，每模型最多 128 条，最多 32 个模型键 |
| 配置错误 | 损坏/不可读文件启动失败 |
| 纯环境 PROXIES | 和文件模式一致，默认直连优先；代理优先需配置文件显式 proxies_enabled=true |
| Rust 内部模块 | debug 迁至 log/context；删除仓库内无调用 API，仓库外调用方需调整 |

超时统一由 config.json 的 upstream_timeout_secs 配置，正整数，默认 20 秒；修改后重启生效。其他限制暂为固定值。持续输出的长任务没有统一总时长硬截止。

## 5. 验证

~~~bash
cargo fmt --check
cargo test --all-targets --offline
cargo clippy --all-targets --offline -- -D warnings
git diff --check
~~~

新增测试：续期分类/更新零行、并发旧快照复用、非 Pro 试用到期、跨 chunk 首包、并发轮询、阻塞区让出调度容量、旧模型缓存、原子探测、并发许可覆盖响应生命周期、过载拒绝、无效配置、大输出禁止重放、网络回退与 HTTP 429 不换通道。

本地 HTTP 测试只证明网关控制流程，不能替代 Raycast 真实 rotation/代理故障实测。本次全量离线测试 203 项通过，累计新增 15 项回归（包括超时默认值/非法值、响应头等待和首包等待）；格式、严格 Clippy 与 diff 空白检查通过。当前环境未发现 Docker CLI，未构建镜像。

## 6. 后续 TODO 与实际限制

* [ ] 多副本共用账号前增加跨进程续期租约/版本条件更新，验证 rotation 故障恢复；当前进程内锁不提供该保证。
* [ ] 真上游验证 401、限额跨分块、代理故障、流空闲超时与恢复。
* [ ] 在具备 Docker 的机器构建、启动，确认 store 挂载权限及 SIGTERM 优雅关闭。
* [ ] 按目标并发和账号数压测 SQLite 锁等待、日志维护与内存，决定是否拆连接或增加有界日志队列。
* [ ] 嵌入 current-thread/LocalSet 前提供异步 Store 或专用数据库线程；当前阻塞区针对项目二进制运行时。
* [ ] 统一列表、计数、选择、删除的账号状态 SQL；保留各接口契约测试，避免为去重引入复杂泛型。
* [ ] 模型缓存职责继续增长时抽出 ModelRegistry；当前 AppState 仍持有缓存，纯解析规则已唯一化。
* [ ] 继续约束异常超大单行 SSE、非流式聚合总量与通知累计；缓存限制不代表整体输出内存有硬上限。

## 7. 扩展规则

新增协议：新增 wire/inbound/outbound 与薄 handler，复用 pipeline，协议特有的流式状态留在适配器。

新增账号规则：先统一分类与持久化决策，再覆盖正常调用、续期后调用、手动刷新三条路径。

新增上游端点：复用 transport，先判断请求是否可安全重放；轮换凭据和有副作用请求不能无条件套用重试。

当前部署基线是单实例 SQLite；跨副本共享账号需要先完成租约与故障恢复验证。
