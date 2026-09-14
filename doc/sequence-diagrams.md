# raycast2api 时序图

> 覆盖三种入口路径 × (非流式 / 流式) 的请求-响应时序。
>
> ✅ 与 Raycast AI 后端交互的部分已按抓包结果落实,详见 [`raycast-backend-protocol.md`](./raycast-backend-protocol.md)。
> **关键前提:Raycast 后端始终以 SSE 流式返回**,请求体无 `stream` 字段。"非流式客户端"由网关消费完整 SSE 后聚合返回。

参与者说明:
- **Client**:Claude Code / Codex / OpenAI 兼容客户端
- **Router**:入口路由层(按路径分发)
- **InAdapter**:请求适配层(协议 → 内部模型)
- **RaycastClient**:Raycast 请求构造 + 调用 + SSE 解析(④⑦)
- **OutAdapter**:响应适配层(内部模型 → 协议)
- **Raycast**:Raycast AI 后端(`POST /api/v1/ai/chat_completions`,始终 SSE)

---

## 1. 非流式(客户端 stream=false)

三条路径非流式流程结构一致,差异仅在 InAdapter / OutAdapter 的协议序列化。
**注意:即便客户端要非流式,网关对后端仍发起 SSE 请求,并在内部把整条流聚合成完整响应。**

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant R as Router
    participant IA as InAdapter
    participant RC as RaycastClient
    participant RAY as Raycast (SSE)
    participant OA as OutAdapter

    C->>R: POST /v1/{messages|responses|chat/completions}<br/>(stream=false)
    R->>R: 按路径确定 protocol
    R->>IA: 转发请求体 + protocol 标签
    IA->>IA: 解析并归一化为 Canonical Request
    IA->>RC: Canonical Request
    RC->>RC: 模型名映射 + 构造 chat_completions 请求体<br/>(content 包 {text}、生成 message_id/thread_id/buffer_id)
    RC->>RAY: POST /api/v1/ai/chat_completions
    RAY-->>RC: SSE 流(reasoning/text 增量 …)
    RC->>RC: 消费整条 SSE 直到 event: complete<br/>拼接文本、归并 tool_calls、汇总 usage
    RC->>OA: Canonical Response + protocol 标签
    OA->>OA: 按 protocol 序列化目标协议 JSON
    OA-->>C: 200 OK + 协议对应响应体
```

**各协议非流式响应体形态**

| 来源路径 | 顶层对象 | 正文字段 | 结束字段 | usage 字段 |
| --- | --- | --- | --- | --- |
| `/v1/messages` | `type: "message"` | `content[].text` | `stop_reason` | `usage.input_tokens/output_tokens` |
| `/v1/responses` | `object: "response"` | `output[].content[].text` | `status` | `usage.input_tokens/output_tokens` |
| `/v1/chat/completions` | `object: "chat.completion"` | `choices[].message.content` | `choices[].finish_reason` | `usage.prompt_tokens/completion_tokens` |

---

## 2. 流式 · Anthropic(`/v1/messages`)

```mermaid
sequenceDiagram
    autonumber
    participant C as Claude Code
    participant R as Router
    participant IA as InAdapter
    participant RC as RaycastClient
    participant RAY as Raycast (SSE)
    participant OA as OutAdapter (Anthropic)

    C->>R: POST /v1/messages (stream=true)
    R->>IA: 请求体 + protocol=messages
    IA->>RC: Canonical Request (stream)
    RC->>RAY: POST /api/v1/ai/chat_completions
    RAY-->>RC: data: {notification_type:provider_status}<br/>data: {ping}
    RC->>OA: 建立 SSE 通道
    OA-->>C: event: message_start

    opt think 模型(model 带 -reasoning)
        loop reasoning 增量
            RAY-->>RC: data: {"reasoning":"…","text":""}
            OA-->>C: content_block_delta<br/>(delta.type=thinking_delta)
        end
        RAY-->>RC: data: {extra_content.anthropic.thought_signature}
        OA-->>C: content_block_delta (signature_delta)
    end

    OA-->>C: event: content_block_start (text, index n)
    loop 正文 text 增量
        RAY-->>RC: data: {"text":"…"}
        OA-->>C: content_block_delta<br/>(delta.type=text_delta)
    end

    alt 工具调用收尾
        RAY-->>RC: data: {finish_reason:"tool_calls",<br/>tool_calls:[{name,arguments,id}], usage}
        OA-->>C: content_block_start/delta (input_json_delta)<br/>message_delta(stop_reason=tool_use)
    else 普通结束
        RAY-->>RC: data: {finish_reason:"stop", usage}
        OA-->>C: content_block_stop<br/>message_delta(stop_reason=end_turn + usage)
    end

    RAY-->>RC: event: complete / data:{complete:true}
    OA-->>C: event: message_stop
```

---

## 3. 流式 · OpenAI Responses(`/v1/responses`)

```mermaid
sequenceDiagram
    autonumber
    participant C as Codex
    participant R as Router
    participant IA as InAdapter
    participant RC as RaycastClient
    participant RAY as Raycast (SSE)
    participant OA as OutAdapter (Responses)

    C->>R: POST /v1/responses (stream=true)
    R->>IA: 请求体 + protocol=responses
    IA->>RC: Canonical Request (stream)
    RC->>RAY: POST /api/v1/ai/chat_completions
    RAY-->>RC: data: {provider_status} / {ping}
    RC->>OA: 建立 SSE 通道
    OA-->>C: event: response.created
    OA-->>C: event: response.output_item.added

    opt think 模型
        loop reasoning 增量
            RAY-->>RC: data: {"reasoning":"…"}
            OA-->>C: event: response.reasoning_summary_text.delta
        end
    end

    OA-->>C: event: response.content_part.added
    loop 正文 text 增量
        RAY-->>RC: data: {"text":"…"}
        OA-->>C: event: response.output_text.delta
    end

    alt 工具调用收尾
        RAY-->>RC: data: {finish_reason:"tool_calls", tool_calls, usage}
        OA-->>C: response.function_call_arguments.delta/.done<br/>response.output_item.done(function_call)
    else 普通结束
        RAY-->>RC: data: {finish_reason:"stop", usage}
        OA-->>C: response.output_text.done<br/>response.content_part.done / response.output_item.done
    end

    RAY-->>RC: event: complete / data:{complete:true}
    OA-->>C: event: response.completed<br/>(含最终 response + usage)
```

---

## 4. 流式 · OpenAI ChatCompletions(`/v1/chat/completions`)

```mermaid
sequenceDiagram
    autonumber
    participant C as OpenAI 兼容客户端
    participant R as Router
    participant IA as InAdapter
    participant RC as RaycastClient
    participant RAY as Raycast (SSE)
    participant OA as OutAdapter (ChatCompletions)

    C->>R: POST /v1/chat/completions (stream=true)
    R->>IA: 请求体 + protocol=chat/completions
    IA->>RC: Canonical Request (stream)
    RC->>RAY: POST /api/v1/ai/chat_completions
    RAY-->>RC: data: {provider_status} / {ping}
    RC->>OA: 建立 SSE 通道
    OA-->>C: data: {choices:[{delta:{role:"assistant"}}]}

    opt think 模型
        loop reasoning 增量
            RAY-->>RC: data: {"reasoning":"…"}
            OA-->>C: data: {choices:[{delta:{reasoning_content}}]}
        end
    end

    loop 正文 text 增量
        RAY-->>RC: data: {"text":"…"}
        OA-->>C: data: {choices:[{delta:{content}}]}
    end

    alt 工具调用收尾
        RAY-->>RC: data: {finish_reason:"tool_calls",<br/>tool_calls:[{name,arguments,id}], usage}
        OA-->>C: data: {choices:[{delta:{tool_calls}}]}<br/>data: {choices:[{finish_reason:"tool_calls"}]}
    else 普通结束
        RAY-->>RC: data: {finish_reason:"stop", usage}
        OA-->>C: data: {choices:[{finish_reason:"stop"}], usage}
    end

    RAY-->>RC: event: complete / data:{complete:true}
    OA-->>C: data: [DONE]
```

---

## 5. 错误处理时序(通用)

```mermaid
sequenceDiagram
    autonumber
    participant C as Client
    participant R as Router
    participant RC as RaycastClient
    participant RAY as Raycast (SSE)
    participant OA as OutAdapter

    alt 请求校验失败
        C->>R: 非法请求(缺字段/模型未知)
        R-->>C: 4xx + 按来源协议的错误体
    else Raycast 后端错误(鉴权/限流/超时)
        RC->>RAY: POST /api/v1/ai/chat_completions
        RAY-->>RC: HTTP 4xx/5xx 或 SSE 中断<br/>(错误体结构待补抓)
        RC->>OA: 归一化错误
        OA-->>C: 按来源协议映射错误<br/>(Anthropic error / OpenAI error 结构)
    end
```

**错误结构映射**

| 来源路径 | 错误响应结构 |
| --- | --- |
| `/v1/messages` | `{ "type": "error", "error": { "type", "message" } }` |
| `/v1/responses` | `{ "error": { "message", "type", "code" } }` |
| `/v1/chat/completions` | `{ "error": { "message", "type", "code" } }` |

> 流式场景下,若已建立 SSE 通道后才出错,需按各协议的流内错误事件(如 Anthropic 的 `event: error`)推送,而非改 HTTP 状态码。

---

## 6. 备注

- 以上时序中 Raycast 侧的事件(`provider_status`/`ping`、`reasoning`/`text` 增量、`tool_calls` 终结、`finish_reason`+`usage`、`event: complete`)均已按抓包 `charles/think_tools.har` 落实,字段细节见 [`raycast-backend-protocol.md`](./raycast-backend-protocol.md)。
- **后端只有 SSE 一种形态**:Client 侧要非流式时,由 RaycastClient 消费整条 SSE 后聚合;要流式时逐事件翻译。两条路径共用同一份 SSE 解析器。
- **provider 无关**:Raycast 出站 SSE 无论 anthropic/openai 都用 `reasoning`+`text` 字段,OutAdapter 只按客户端来源协议分支。
- 三种协议的 SSE 事件模型差异集中在 OutAdapter,是本项目转换逻辑的核心复杂点,建议实现时对每种协议单独编写事件状态机。
- 仍待补抓:`x-raycast-signature-v2` 签名算法、完整模型清单、后端错误响应结构。
