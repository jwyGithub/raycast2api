# 上游调用统计

`statistics` 与账号、日志共用 `accounts_db` 指定的 SQLite 库，启动自动建表和索引。统计没有自动过期清理，不会随日志的 7 天保留策略删除；不从历史日志回填数据。

## 统计口径

一条记录代表一次实际发送给上游的推理尝试，`id` 是独立 ULID。一个客户端请求的所有尝试共享 `request_id`，可与请求日志关联：

* `call_type = inference`：正式推理。
* `call_type = compact`：Responses 上下文压缩摘要。缓存命中的摘要不调用上游，因此不产生新记录。
* 换号、token 续期后重发、直连与代理间切换、断流重试分别生成新记录。OAuth 刷新、模型列表与账号额度查询不纳入推理统计。
* 鉴权失败、解析失败、未选到账号等没有进入实际发送阶段的请求不生成统计。

客户端请求数应按 `COUNT(DISTINCT request_id)` 计算，上游调用次数按行数计算；费用汇总应包含摘要和失败尝试中已报告的 usage。

## 字段

| 字段 | 说明 |
| --- | --- |
| `id` | 上游调用 ID：网关每次实际向上游发送调用时生成，唯一标识一条统计记录；详情按此字段查询 |
| `request_id` | 请求 ID：网关收到客户端推理请求时生成，同一次请求的摘要、重试等上游调用共享此 ID，可用于关联请求日志 |
| `api_key`、`api_key_masked` | 请求时使用的原始 key 与脱敏值；Bearer 优先，`x-api-key` 次之；无 key 时为 `null`。长度大于 8 时保留前后各 N 个字符（N 为长度的四分之一，最多 8），否则显示 `****` |
| `requested_at`、`started_at`、`finished_at` | 客户端到达、尝试开始、尝试完成时间；数据库为 UTC 秒级整数，接口为 `Asia/Shanghai` 的 `YYYY-MM-DD HH:mm:ss` 字符串；未完成时 `finished_at` 为 `null` |
| `request_ip` | 连接 IP；配置可信反向代理后可解析转发链。缺少连接信息时为 `null` |
| `account_email` | 本次发送实际选用的账号邮箱快照；后续删除账号不删除统计 |
| `requested_model` | 客户端模型 ID（省略时为网关默认值），模型别名应用前的值 |
| `upstream_provider`、`upstream_model` | 最终发送的请求体中的 provider 和模型，不代表上游内部降级后的实际执行模型 |
| `requested_effort` | 客户端原始 effort 字符串，例如 `max`；缺省为 `null` |
| `requested_thinking_json` | 仅记录 Messages 请求显式传入的 thinking 类型与 budget_tokens，接口返回 JSON 对象；未传或使用其他协议时为 `null`。不记录模型返回的思考内容，推理强度另见 `requested_effort` 和 `upstream_effort` |
| `upstream_effort` | 实际发送的 reasoning_effort，例如 `xhigh`；不发送时为 `null`。摘要会保留所属客户端原始配置，并单独记录摘要的上游配置 |
| `call_type`、`status` | 调用类型与状态，见下表 |
| `http_status` | 实际上游 HTTP 状态；收到 HTTP 响应前失败为 `null`。HTTP 200 流内失败仍记 200，`status` 为 `failed`，原因保存在 `error_message` |
| `error_message` | 错误、取消或中断原因；成功时为 `null` |
| `usage_json` | 上游完整 usage 对象，保留 token、cost、credits 及未知扩展字段；接口返回 JSON 对象，未收到时为 `null` |

| 状态 | 含义 |
| --- | --- |
| `in_progress` | 已创建尝试，尚未结算 |
| `completed` | 收到上游 Finish；没有 usage 仍保持 `usage_json: null` |
| `failed` | HTTP / 网络请求失败，或 SSE 内明确报告错误 |
| `cancelled` | 调用任务、响应或流在终止事件前被丢弃，例如客户端断开 |
| `interrupted` | SSE 没有终止事件就结束，包括 EOF、读超时、传输错误；或服务重启时发现上次未完成记录 |

结算以上游调用 ID 幂等更新；已完成行不会被重复回调或流释放覆盖。完整 usage 在上游解析边界采集，终止事件在交给客户端协议适配器前结算，不依赖日志内容或其截断上限。

## 查询接口

两个接口都支持 Dashboard Session 或 `api_key` 鉴权，响应使用 `{code, message, data}` 管理接口包络。

### 分页明细

`POST /v1/statistics/list`，空对象 `{}` 使用第一页、每页 10 条，按 `started_at`、`id` 降序。所有筛选以 AND 组合，字符串精确匹配，时间范围包含两端。

```json
{
    "page": 1,
    "page_size": 20,
    "requested_at_from": 1794787200,
    "requested_at_to": 1794873599,
    "request_id": "01EXAMPLE_REQUEST_ID",
    "account_email": "account@example.com",
    "requested_model": "openai-gpt-6.1-sol",
    "upstream_model": "gpt-6.1-sol",
    "api_key": "<请求时使用的 api_key>",
    "call_type": "inference",
    "status": "completed"
}
```

所有字段可省略。`page >= 1`，`page_size` 为 1 到 100。时间筛选使用非负 UTC Unix 秒，起始值不得大于结束值；非法分页和时间范围返回 400，字段类型错误或未知字段返回 422。没有匹配记录时返回空列表。

`data` 包含 `items`、`total`、`page`、`page_size`、`total_pages`，每个 `items` 元素包含上表全部字段，`usage_json` 为对象或 `null`。

### 详情

`POST /v1/statistics/detail`：

```json
{
    "id": "<统计列表中的 id>"
}
```

`data` 返回单次尝试的全部字段；不存在时返回 404，数据库错误返回 500。

## 来源 IP

服务的 IPv4 / IPv6 监听均注入连接地址，直接记录连接 IP，不解析 `X-Forwarded-For` 等转发头。
