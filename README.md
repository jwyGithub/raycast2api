# raycast2api

把 **Raycast AI 后端**包装成标准的、与 OpenAI / Anthropic 兼容的 HTTP 网关。用 Rust（Axum + Tokio）编写，让 Claude Code、Codex 以及各类 OpenAI 兼容客户端可以直接把请求打到本地网关，由网关负责协议转换、请求签名与流式响应聚合。

> 上游接口 `POST https://backend.raycast.com/api/v1/ai/chat_completions` **始终以 SSE 流式返回**。网关的"非流式"响应是把整条流聚合后一次性返回的结果。

## 特性

- **三种客户端协议，一个上游**：按**请求路径**判定协议（不做请求头/字段探测），分别适配 OpenAI Chat Completions、Anthropic Messages、OpenAI Responses。
- **流式与非流式**：三条路径都同时支持 SSE 流式与聚合后的非流式响应。
- **请求签名**：自动计算 Raycast 所需的 `X-Raycast-Signature-v2` 头（ROT13/ROT5 + HMAC-SHA256）。
- **模型直通**：`GET /v1/models` 透出账号可用模型，可选择是否包含高级 / 已弃用模型。
- **请求落盘排查**：设置 `DEBUG_DUMP_DIR` 后，每个请求的入站体、上游请求体与上游 SSE 响应会存为文件，便于离线分析。
- **可选鉴权**：设置 `api_key` 后，所有推理与只读接口都需要携带该 key。
- **双栈监听**：绑定 `0.0.0.0` 时自动同时监听 IPv6 `[::]`。

## 架构

网关采用"协议适配层 → 统一内部模型 → 上游适配层"的分层设计，客户端协议与上游 Raycast 互不感知，任意一侧都可独立演进：

```
客户端协议  ──②inbound──▶  ③统一内部请求  ──④构造──▶  Raycast 后端(SSE)
(①路由)                                                      │
客户端协议  ◀─⑤outbound──  ⑥统一内部响应  ◀─⑦解析──────────────┘
```

- `config` — 静态端点常量与运行时 `Settings`
- `core` — 协议 / 供应商无关的**统一内部模型**（③⑥）
- `protocol` — 各客户端协议的 inbound/outbound 适配器（②⑤）
- `raycast` — 唯一与上游耦合的层：请求构造器、SSE 解析器、签名客户端（④⑦）
- `signature` — `X-Raycast-Signature-v2` 计算
- `server` — HTTP 层：状态、中间件、路由、handler
- `debug` — 请求落盘（`DEBUG_DUMP_DIR`）与请求关联 id

详细设计与时序图见 [`doc/architecture.md`](./doc/architecture.md)、[`doc/sequence-diagrams.md`](./doc/sequence-diagrams.md)，上游抓包协议见 [`doc/raycast-backend-protocol.md`](./doc/raycast-backend-protocol.md)。

## 支持的接口

| 路径 | 协议 | 典型客户端 |
| --- | --- | --- |
| `POST /v1/messages` | Anthropic Messages | Claude Code |
| `POST /v1/responses` | OpenAI Responses | Codex |
| `POST /v1/chat/completions` | OpenAI Chat Completions | 各类 OpenAI 兼容客户端 |
| `GET /v1/models` | 模型列表 | — |
| `GET /health`, `GET /ready` | 健康检查 | — |

## 环境要求

- Rust（edition 2024，建议使用最新稳定版工具链）
- 一个有效的 Raycast 账号，用于获取 `bearer_token` 与 `device_id`

## 构建与运行

```bash
# 构建
cargo build --release

# 运行（默认监听 0.0.0.0:8787）
cargo run --release
```

启动前需要提供配置（见下文）。若未设置 `api_key`，网关会接受**未鉴权**请求并打印告警。

## 配置

配置加载顺序：优先读取项目根目录的 `config.json`；**环境变量始终覆盖**文件中的同名项。若没有 `config.json`，则完全从环境变量读取。

复制示例文件并填入你的 Raycast 凭据：

```bash
cp config.example.json config.json
```

```json
{
  "host": "0.0.0.0",
  "port": 8787,
  "api_key": "your-api-key-here",
  "raycast_bearer_token": "your-raycast-bearer-token",
  "raycast_device_id": "your-raycast-device-id",
  "raycast_signature_secret": null,
  "show_advanced": true,
  "include_deprecated": true
}
```

### 配置项 / 环境变量对照

| 配置项 | 环境变量 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `raycast_bearer_token` | `RAYCAST_BEARER_TOKEN` | 是 | — | Raycast `Authorization: Bearer` 令牌 |
| `raycast_device_id` | `RAYCAST_DEVICE_ID` | 是 | — | Raycast 设备 ID，同时参与签名计算 |
| `raycast_signature_secret` | `RAYCAST_SIGNATURE_SECRET` | 否 | 内置默认密钥 | 签名密钥，一般无需设置 |
| `api_key` | `API_KEY` | 否 | 无 | 网关自身的 API key；不设置则不鉴权 |
| `host` | `HOST` | 否 | `0.0.0.0` | 监听地址（须为 IP） |
| `port` | `PORT` | 否 | `8787` | 监听端口 |
| `show_advanced` | `ADVANCED` | 否 | `true` | `/v1/models` 是否包含高级（premium）模型 |
| `include_deprecated` | `INCLUDE_DEPRECATED` | 否 | `true` | `/v1/models` 是否包含已弃用模型 |

日志级别通过 `RUST_LOG` 控制（基于 `tracing-subscriber` 的 env-filter）。

## 使用示例

以下示例假设网关运行在 `http://localhost:8787`。若设置了 `api_key`，请加上 `-H "Authorization: Bearer <你的 api_key>"`。

OpenAI Chat Completions（流式）：

```bash
curl http://localhost:8787/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "openai-gpt-4o-mini",
    "stream": true,
    "messages": [{"role": "user", "content": "你好"}]
  }'
```

Anthropic Messages：

```bash
curl http://localhost:8787/v1/messages \
  -H "Content-Type: application/json" \
  -d '{
    "model": "openai-gpt-4o-mini",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "你好"}]
  }'
```

列出可用模型：

```bash
curl http://localhost:8787/v1/models
```

## 测试

```bash
cargo test
```

`tests/` 下覆盖了三种协议的契约测试、消息转换、错误映射、ID 派生以及签名一致性（`signature_parity*`，基于 `charles/*.har` 抓包对齐上游签名）。

## 项目结构

```
src/
  config.rs        运行时配置与静态端点常量
  signature.rs     X-Raycast-Signature-v2 计算
  core/            统一内部请求/响应/事件模型
  protocol/        chat_completions / messages / responses 三套适配器
  raycast/         上游客户端、请求构造、SSE 解析、模型解析
  server/          路由、中间件（鉴权/调试捕获）、handler、状态
  debug.rs         请求落盘与关联 id
doc/               架构、时序图、上游协议抓包说明
tests/             契约测试与签名一致性测试
```

## 说明

- `bearer_token`、`device_id`、`api_key` 等属于敏感信息，`config.json` 与 `.env` 已在 `.gitignore` 中忽略，请勿提交。
- 落盘目录（`DEBUG_DUMP_DIR`）包含完整请求与上游凭证相关数据，**切勿**将其暴露或提交到仓库。
