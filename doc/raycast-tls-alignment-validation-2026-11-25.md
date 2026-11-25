# TLS 指纹对齐：实施与验收记录

日期：2026-11-25。设计依据：[实现方案](raycast-tls-alignment-implementation.md)；目标快照：[原始指纹记录](raycast-tls-fingerprint-2026-11-22.md)。

已实现常驻 Node 上游传输后端。Rust 保留签名、账号调度、代理通道选择、Attempt 统计和 SSE 转换；Node 接收签名后的原始字节，执行一次指定通道的请求。默认仍为 `native`，启用 `node` 后目标站点的 TLS 由固定 Node/OpenSSL 发起。

## 1. 启用方式

### Docker 部署

本次修改后的 Dockerfile 会下载并验证 Node 26.13.1 官方 Linux 发行包，使用 lockfile 安装 undici 8.12.4。需要重新构建镜像，已有发布镜像不会自动获得这些文件。

在原有有效 `config.json` 中增加或修改以下块，保留其他配置：

```json:config.json
{
  "upstream_transport": {
    "backend": "node",
    "profile": "raycast-windows-2.13.1.0",
    "node_path": "/opt/node/bin/node",
    "transport_dir": "/app/transport/node",
    "startup_timeout_secs": 15
  }
}
```

容器无需新增端口映射。Rust 启动 Node 后监听随机 loopback 端口，使用进程级随机令牌鉴权。代理及令牌通过 stdin 初始化，不写入命令参数。

### 本地部署

安装官方 **Node 26.13.1**，确认 `node`/`npm` 指向这套运行时。依赖版本固定，不能以系统 Node 主版本相近作为替代。然后在项目根目录执行：

```bash:项目根目录
node --version
npm ci --prefix transport/node --ignore-scripts --no-audit --no-fund
node transport/node/fingerprint.mjs
```

在原有配置中设置 `backend: "node"`。`node_path` 建议填固定 Node 二进制的绝对路径；`transport_dir` 建议填本仓库 `transport/node` 的绝对路径。配置示例中的相对路径以网关工作目录为基准。

启动时会检查 Node、OpenSSL、内置/外置 undici、nghttp2 版本，并捕获本机实际 ClientHello 校验两套指纹及外置 undici 的 ALPN。校验失败时网关拒绝启动；不自动换回 native。校验只访问本机，不向公网发送账户数据。

回滚时将 `backend` 改为 `native` 并重启网关。配置在启动时加载。本次开发未修改用户实际 `config.json`，也未重启现有服务。

## 2. 接入范围与行为

| 路径 | Node 管道 | 请求策略 |
| --- | --- | --- |
| `/ai/chat_completions` | 外置 undici fetch | ALPN 为 `http/1.1,h2`，服务端可选 HTTP/2；通道切换与有限重试由 Rust 控制 |
| 模型、账号信息、额度、文件注册、OAuth | Node http/https | HTTPS 不设置 ALPN，使用 HTTP/1.1；保留上游 HTTP 状态 |
| 预签名文件 PUT | Node http/https | 遵守代理模式，只使用一个通道；不跟随重定向或重放 |
| 生成图片下载 | 外置 undici fetch | 使用 Rust 选出的下载通道，保留现有 60 秒请求总超时及 20 MiB 限制 |

两组 Agent/dispatcher 按管道和代理索引隔离。直连不读取环境代理；HTTP/HTTPS 使用 CONNECT，SOCKS5 保持 `socks5://` 的本地 DNS 语义。代理认证仅用于隧道协商。

内部协议使用 `4 字节大端长度 + JSON 元数据 + 原始 body`，元数据上限 64 KiB。请求 body 不重新解析 JSON；响应在元数据后按流转发。上游 429/500 等仍返回其实际状态。gzip、deflate、br、zstd 只解压一次，并移除与解压后 body 不匹配的编码和长度头。

GET/HEAD 可以跟随最多 10 次重定向；跨 origin 移除 Authorization、Cookie 和 `X-Raycast-*`，拒绝 HTTPS 降级。POST/PUT 不自动跟随或重放。OAuth 固定单通道，避免轮换令牌时发生模糊失败重试。

流式响应使用 pipeline 背压。取消本机响应会中止对应上游请求；部分 SSE 后断流仍是流错误，由现有 Rust 逻辑保留内容并明确报错。连接/响应头有超时，流式持续时间不被 Node 全局 body 超时截断。

Node 异常退出后请求快速返回不可用；进程管理器最多重启三次，退避 1/2/4 秒，在途请求不恢复。`/ready` 先检查本机 Node 服务，再执行现有账号和后端检查。SIGTERM/Ctrl-C 停止接收新请求，5 秒排空窗口后取消 Node 流，随后关闭子进程；父进程 stdin 关闭也会退出。

## 3. 实际指纹结果

测试平台：macOS arm64，官方 Node 26.13.1，OpenSSL 3.5.9，内置及外置 undici 8.12.4，nghttp2 1.70.0。

| 项目 | ChatFetch 实测 | Got 实测 |
| --- | --- | --- |
| JA3 | `d67b094811e5145139d7cea5f014309f` | `944d1e1858cd278718f8a46b65d3212f` |
| 本地规范 JA4 | `t13d5212h1_b262b3658495_8e6e362c5eac` | `t13d521100_b262b3658495_8e6e362c5eac` |
| ALPN | `http/1.1,h2` | 无 ALPN 扩展 |
| 公网协商协议 | HTTP/2 | HTTP/1.1 |
| HTTP/2 Akamai | `2:0;4:262144\|458753\|0\|a,m,p,s` | 不适用 |
| Akamai MD5 | `75560cd255283a449819f60ea47983aa` | 不适用 |

ChatFetch 的 JA3、JA4、Akamai 均与目标快照一致；Got 的 JA3 一致且实际不存在 ALPN 扩展。公网 tls.peet.ws 的 Got JA4 输出 `t13d5211_b262b3658495_8e6e362c5eac`，与原文相同；本地解析器按 JA4 无 ALPN 的 `00` 规则输出完整格式。回显脚本接受这两种明确列出的格式。

已归档：

- [目标 profile](../transport/node/profiles/raycast-windows-2.13.1.0.json)。
- [本机握手字段与运行时记录](../transport/node/profiles/darwin-arm64-2026-11-25.json)，含 cipher 顺序、扩展、groups、signature algorithms、supported versions、key share 组与长度。
- [公网回显记录](../transport/node/profiles/darwin-arm64-echo-2026-11-25.json)，只保留指纹与运行时字段。

本次 macOS 官方压缩包 SHA256 为 `aec2514ea1ceb2ba52455e9ec65986f6749a06dd759e5db2a72e6889668c9735`。Linux x64/arm64 官方包的固定摘要见 [安装脚本](../transport/node/install-runtime.sh)。摘要来源：[Node 官方校验文件](https://nodejs.org/dist/v26.13.1/SHASUMS256.txt)。

## 4. 回归与复验

Node 的 16 项测试分别在 IPv4 优先和 IPv6 优先的 DNS 配置下全部通过，覆盖真实双管道握手、直连与三种认证代理、IPv4/IPv6 SOCKS5 隧道、HTTP/2 协商、原始签名字节/二进制上传、四种压缩编码、429 状态、部分 SSE 断流、取消、重定向凭据、本机鉴权和按请求设置响应头截止时间。

Rust 跨进程测试启动真正的固定 Node，覆盖状态/body/header/SSE 错误、代理 fallback、OAuth 单次执行、Node 崩溃后的快速失败、重启不重放及关闭。它们默认标记 ignored，必须用下列命令显式执行，不能用普通 cargo test 的成功代替。

本次常规 Rust 测试共 342 项通过，6 项默认 ignored；另行显式执行的三项 Node 跨进程测试全部通过。Clippy 严格检查、rustfmt、安装脚本与 CI shell 语法检查通过。

使用隔离的临时配置和空账号数据库启动当前网关二进制，确认 native 无 Node 也可启动、node 模式健康检查成功且 `/ready` 进入空账号分支、两种模式均正常响应 SIGTERM 并以 0 退出；系统 Node 24.13.0 在 node 模式下被指纹/版本校验拒绝，网关以 1 退出。未访问真实 Raycast 账号。

```bash:项目根目录
cargo test --locked --quiet
node --dns-result-order=ipv4first --test transport/node/test/*.test.mjs
node --dns-result-order=ipv6first --test transport/node/test/*.test.mjs
RAYCAST_TEST_NODE_PATH=/绝对路径/node cargo test --locked --lib raycast::node_transport::tests -- --ignored
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

可选公网复验只发送无凭据 GET；它同时检查 JA3/JA4 和 ChatFetch 的 Akamai 指纹，差异会返回非零退出码：

```bash:项目根目录
node transport/node/echo.mjs --output /tmp/raycast-tls-echo.json
```

Docker 构建会执行 Node 指纹与 IPv4/IPv6 两种 DNS 顺序的传输测试、Rust 常规测试及三项跨进程测试，最终以运行用户再次执行指纹检查。[CI 工作流](../.github/workflows/transport-check.yml) 在 PR/main/master 相关文件变更后构建本地镜像，检查 Node 模式下的网关启动、健康检查、账号就绪分支和正常退出，不发布镜像。

## 5. 验证边界

| 场景 | 当前证据 |
| --- | --- |
| macOS arm64 新连接双管道 | 本机实际 ClientHello 与公网 TLS/h2 回显通过 |
| HTTP/HTTPS/SOCKS5 代理 | 本地真实认证隧道及隧道内双管道 ClientHello 通过；未测用户的实际公网代理出口 |
| 连接池复用 | 本地连续多请求、取消后继续请求通过；没有与真实客户端会话恢复握手做逐字段比较 |
| Rust 业务协议 | 常规契约回归与三项真实跨进程测试通过 |
| Linux 镜像 | CI 已通过 Node、Rust 测试并完成网关启动与就绪检查；运行时 GLIBC 弱版本警告已修复基底并增加检查，本机无 Docker，新镜像仍待 CI 重跑 |
| 真实 Raycast 账号流程 | 尚未执行 models/me/OAuth/文件上传/聊天的真实上游验收 |
| 完整流量形态与负载 | 未做会话恢复、所有 HTTP/2 帧/HPACK 行为及大规模并发内存的完整比对 |

这些证据支持已测平台上的首次握手和 HTTP/2 摘要对齐；不能推定所有部署架构或实际业务流量完全一致，也不构成上游风控行为的验证。

## 6. CI 构建修复

构建失败中的 `4 !== 1` 是 SOCKS5 地址类型差异：`1` 表示 IPv4，`4` 表示 IPv6。Linux 的 `localhost` 可优先解析为 `::1`，原测试代理只接受 IPv4；抓取 ClientHello 的服务也只监听 `127.0.0.1`。

修复后，测试代理按地址类型等待并解析完整的 4/16 字节地址及端口。抓包服务在同一端口分别监听 `127.0.0.1` 和可用的 `::1`，保留 `localhost` 的 TLS SNI，仅开放回环地址。生产 SOCKS5 的本地 DNS 策略保持不变。Docker 构建同时执行 IPv4/IPv6 优先两组测试，防止仅在开发机 DNS 顺序下通过。

Rust 构建阶段新增 `cargo test` 后，需要完整的 `tests/` 源码。原 `.dockerignore` 排除了该目录，导致 `#[path]` 引用的 `tests/unit/compact_redis.rs`、`statistics.rs` 等文件在镜像内缺失。已移除该排除规则，保留单元与集成测试文件。最终运行阶段仍仅复制 Rust 二进制、Node 运行时及 Node 传输目录，不复制 Rust 测试源码。

后续 CI 已成功启动网关，但输出 `weak version GLIBC_2.39 not found`。Rust 构建镜像原先使用浮动的 `rust:1`，官方镜像清单当前将其指向 Trixie，运行阶段则为 Bookworm。已将构建镜像改为 `rust:1-bookworm`，使构建、Node 和运行阶段均使用 Bookworm；运行阶段执行 `ldd --verbose` 检查动态库及符号版本，并拒绝任何 `not found` 提示，包含不阻止进程启动的弱版本警告。镜像标签依据：[Rust 官方镜像清单](https://github.com/docker-library/official-images/blob/master/library/rust)。

CI 的 `docker stop --time 15` 同时更新为 `docker stop --timeout 15`，保留 15 秒正常退出窗口。空账号配置下 `/ready` 返回 503 且消息为 `No available accounts` 是预期断言；健康检查成功和正常退出仍分别验证。
