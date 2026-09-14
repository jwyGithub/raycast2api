# Raycast AI 后端协议(抓包实录)

> 来源:Charles 抓包 `charles/think_tools.har`(Raycast 2.1.3.0,Windows;含 think / 非 think、多轮工具调用)。
> 本文是 ④ Raycast 请求构造器 与 ⑦ Raycast 响应解析器 的**权威依据**,替换了架构文档中的所有 `【待抓包】` 占位。

---

## 1. 接口概览

| 项 | 值 |
| --- | --- |
| Method / URL | `POST https://backend.raycast.com/api/v1/ai/chat_completions` |
| 请求体 | `application/json` |
| 响应体 | **始终** `text/event-stream`(SSE)。请求体中**没有** `stream` 字段 —— Raycast 后端**永远流式**返回 |
| 鉴权 | `Authorization: Bearer rca_<token>` |

**关键结论:后端只有一种交互形态 —— SSE 流式。** 网关无论面对流式还是非流式的客户端,都对后端发起同一种流式请求;"非流式"由网关自己把流**聚合**成完整响应后一次性返回给客户端。

### 1.1 请求头

```
:method: POST
:authority: backend.raycast.com
:path: /api/v1/ai/chat_completions
content-type: application/json
accept: application/json
authorization: Bearer rca_c31b9b9d014a5f387c17baae8bad603a...   # Raycast 账号 token
user-agent: Raycast/2.1.3.0 (Windows-amd64 10.0.26100.0)
origin: file://
x-raycast-deviceid: 69e4a49f117f3fc450970299a733d0bbe461a142e09fb54c9c6ded895f0e2f67
x-raycast-timestamp: 1788423597
x-raycast-signature-v2: 3ac48379a6eba86da600309829f5cf56e9bf2cf1a79aebbb2a581f225654d495
x-raycast-experimental: autoModels
accept-encoding: gzip, deflate, br, zstd
accept-language: zh-CN,zh;q=0.9,en;q=0.8,...
```

> ⚠️ **鉴权与签名**:`Authorization` 是 Raycast 账号 token(以 `rca_` 开头)。`x-raycast-signature-v2` + `x-raycast-timestamp` + `x-raycast-deviceid` 是 Raycast 客户端对请求做的签名。网关作为中间层,需要**透传/复用**一份有效的 Raycast token(以及必要时的 deviceid/签名)。签名算法本次抓包无法反推,实现时通常做法是让用户提供自己的 Raycast token,并复用其 deviceid/签名头。

---

## 2. 请求体结构

### 2.1 顶层字段

```jsonc
{
  "system_instructions": "You are a helpful assistant...",        // 可选,基础系统提示
  "additional_system_instructions": "<memory_system>...</...>",   // 追加系统提示(记忆/扩展/技能说明)
  "locale": "zh-CN",
  "temperature": 0,
  "current_date": "2026-09-03",
  "message_id": "822b3512-9019-42b0-81d9-8f8c1098ecf4",           // UUID,本轮用户消息 id
  "thread_id": "01M1K5M5YJKDNK4609XWEBT638",                       // 会话线程 id(ULID)
  "reasoning_effort": "low",                                       // ★ 仅 think 模型出现:low|medium|high
  "messages": [ /* 见 2.2 */ ],
  "tools": [ /* 见 2.3 */ ],
  "tool_choice": "auto",
  "source": "ai_chat",                                             // ai_chat | ai_chat_title(标题生成)
  "model": "claude-haiku-4-5-20251001-reasoning",                  // 见 2.4
  "provider": "anthropic",                                         // anthropic | openai
  "buffer_id": "accc8579-ffa2-4790-bfef-8bb54f0ad922"              // UUID
}
```

### 2.2 `messages[]` —— 消息结构

**重点:`content` 永远是对象 `{"text": "..."}`,不是裸字符串。**

| 类型 | 结构 |
| --- | --- |
| 用户消息 | `{"role":"user","content":{"text":"..."}}` |
| 助手文本 | `{"role":"assistant","content":{"text":"..."}}` |
| 助手思考 | 作为**独立的一条 assistant 消息**,`content.text` 用 `<think>...</think>` 包裹整段思考内容 |
| 助手工具调用 | `{"role":"assistant","content":{"text":""},"tool_calls":[...],"extra_content":{...}}` |
| 工具结果 | `{"role":"tool","content":{"text":"..."},"name":"<工具名>","tool_call_id":"<id>"}` |

助手工具调用(OpenAI 风格,`arguments` 是 **JSON 字符串**):

```jsonc
{
  "role": "assistant",
  "content": {"text": ""},
  "tool_calls": [{
    "id": "toolu_017SbszW3dLjwMdQbqEUhbc3",
    "type": "function",
    "function": {
      "name": "ask_user_question",
      "arguments": "{\"question\":\"...\",\"options\":[\"...\"]}"   // 字符串,非对象
    }
  }],
  "extra_content": {                                    // 仅 think 模型:回传思考签名
    "anthropic": { "thought_signature": "ErQFCrIB..." }  // provider=openai 时为 { "openai": {...} }
  }
}
```

工具结果消息(`content.text` 内是工具返回的 JSON 字符串):

```jsonc
{
  "role": "tool",
  "content": {"text": "[{\"status\":\"completed\"},{\"text\":\"...\",\"type\":\"text\"}]"},
  "name": "ask_user_question",
  "tool_call_id": "toolu_017SbszW3dLjwMdQbqEUhbc3"
}
```

### 2.3 `tools[]` —— 工具定义

```jsonc
// 本地工具:带完整 JSON Schema
{"type":"local_tool","function":{"name":"...","description":"...","parameters":{ /* JSON Schema */ }}}
// 远程工具:只给名字(如 web_search / search_images)
{"type":"remote_tool","name":"web_search"}
```

### 2.4 `model` / `provider` / think 判定

| provider | model 示例 | think? |
| --- | --- | --- |
| `anthropic` | `claude-haiku-4-5-20251001-reasoning` | 是(`-reasoning` 后缀 + 有 `reasoning_effort`) |
| `openai` | `gpt-5.6-luna` | 否(无后缀、无 `reasoning_effort`) |

**think(思考)与非 think 的差异集中在两处:**
1. **请求**:think 模型名带 `-reasoning` 后缀,且携带 `reasoning_effort`(low/medium/high);非 think 两者都没有。
2. **响应**:think 会先流一段 `reasoning` 增量并回传 `thought_signature`;非 think 只流 `text`。

---

## 3. 响应结构(SSE)

`Content-Type: text/event-stream`。事件以空行分隔;正文事件带 `id: <递增序号>`,通知事件无 id。

### 3.1 事件类型一览

| data 内容 | 含义 |
| --- | --- |
| `{"notification_type":"provider_status","status":"operational","status_page_url":"...","text":""}` | 起始:后端/上游状态 |
| `{"notification_type":"ping","notification":"","text":""}` | 保活 ping(全程可能穿插多次) |
| `{"reasoning":"<增量>","text":""}` | **思考增量**(think 模型) |
| `{"reasoning":"","text":"","extra_content":{"anthropic":{"thought_signature":"..."}}}` | 思考结束,回传签名(provider=openai 时键为 `openai`) |
| `{"text":"<增量>"}` | **正文增量** |
| `{"finish_reason":"tool_calls","text":"","tool_calls":[...],"usage":{...}}` | 以**工具调用**收尾 |
| `{"text":"","finish_reason":"stop","usage":{...}}` | 以**普通文本**收尾 |
| `event: complete` + `data: {"complete":true}` | **流终止标记** |

### 3.2 普通文本流(think 模型)示例

```
id: 0
data: {"notification_type":"provider_status","notification":"","status":"operational","status_page_url":"https://status.claude.com","text":""}

data: {"notification":"","notification_type":"ping","text":""}

id: 1
data: {"reasoning":"用","text":""}
id: 2
data: {"reasoning":"户让","text":""}
...
id: 14
data: {"reasoning":"","text":"","extra_content":{"anthropic":{"thought_signature":"EuwICrIB..."}}}

id: 15
data: {"text":"# 分析：GLM 模型 API 测试脚本\n\n..."}
id: 16
data: {"text":"系列模型 API 的 Python 脚本..."}
...
id: 29
data: {"text":"","finish_reason":"stop","usage":{"input_tokens":5574,"output_tokens":891,"cached_tokens":0,"input_cost":0.006965,"output_cost":0.004455,"total_cost":0.01142}}

event: complete
data: {"complete":true}
```

### 3.3 工具调用收尾示例

工具调用**不逐字符流式**,而是在一个终结事件里整包给出(`arguments` 为 JSON 字符串):

```
id: 1
data: {"text":"我可以帮你添加 glm-5.1 模型的测试。请问..."}

id: 2
data: {"finish_reason":"tool_calls","text":"","tool_calls":[{"name":"ask_user_question","arguments":"{\"question\": \"glm-5.1 模型测试采用什么方案？\", \"options\": [\"...\"]}","id":"toolu_017SbszW3dLjwMdQbqEUhbc3"}],"usage":{"input_tokens":6453,"output_tokens":209,"cached_tokens":0,"input_cost":0.0080655,"output_cost":0.001045,"total_cost":0.0091105}}

event: complete
data: {"complete":true}
```

> 注:SSE 流里的 tool_call 结构是 `{name, arguments, id}`(扁平);而写回**请求** `messages[].tool_calls` 时是 `{id, type:"function", function:{name, arguments}}`(OpenAI 嵌套式)。网关回程需做这层结构转换。

### 3.4 `usage` 字段

```jsonc
"usage": {
  "input_tokens": 5574,
  "output_tokens": 891,
  "cached_tokens": 0,      // 命中缓存的输入 token
  "input_cost": 0.006965,  // 美元
  "output_cost": 0.004455,
  "total_cost": 0.01142
}
```

`finish_reason` 取值:`stop`(正常结束)、`tool_calls`(需要执行工具)。

---

## 4. 对网关(raycast2api)的映射要点

### 4.1 入站(客户端协议 → Raycast 请求)

| Canonical 字段 | → Raycast 字段 |
| --- | --- |
| system 提示(Anthropic `system` / Responses `instructions` / ChatCompletions `system` 消息) | `system_instructions`(+ 可选 `additional_system_instructions`) |
| 普通消息 role+文本 | `messages[].role` + `content:{text}`(**务必包成对象**) |
| 思考历史 | 作为独立 assistant 消息,`content.text` = `<think>...</think>` |
| 助手工具调用 | `messages[].tool_calls[]`(OpenAI 嵌套式,`arguments` 转成字符串) |
| 工具结果 | `{role:"tool", content:{text}, name, tool_call_id}` |
| tools 定义 | `tools[]`(本地 `local_tool.function` / 远程 `remote_tool.name`) |
| 模型名 | 映射到 Raycast `model` + `provider`;think 需求 → 选带 `-reasoning` 的模型并补 `reasoning_effort` |
| temperature | `temperature` |
| — | 需生成:`message_id`(UUID)、`thread_id`(ULID)、`buffer_id`(UUID)、`current_date`、`locale`、`source:"ai_chat"` |

### 4.2 出站(Raycast SSE → 客户端协议)

网关统一消费 3.1 的事件,再按来源协议序列化:

| Raycast 事件 | Anthropic (`/v1/messages`) | Responses (`/v1/responses`) | ChatCompletions (`/v1/chat/completions`) |
| --- | --- | --- | --- |
| `reasoning` 增量 | `content_block_delta`(`thinking_delta`) | `response.reasoning_summary_text.delta` | `choices[].delta.reasoning_content`(扩展) |
| `text` 增量 | `content_block_delta`(`text_delta`) | `response.output_text.delta` | `choices[].delta.content` |
| `tool_calls` 终结 | `content_block_start/delta`(`input_json_delta`) | `response.function_call_arguments.delta` | `choices[].delta.tool_calls` |
| `finish_reason:stop` + usage | `message_delta`(stop_reason+usage) → `message_stop` | `response.completed` | `choices[].finish_reason` → `data:[DONE]` |
| `event: complete` | 流关闭 | 流关闭 | 流关闭 |

- **非流式客户端**:网关消费完整条 SSE(直到 `event: complete`),把 `reasoning`/`text` 拼接、tool_calls 归并、usage 汇总,再一次性序列化返回。
- **provider 无关性**:无论 `anthropic` 还是 `openai`,Raycast 出站 SSE **格式一致**(都用 `reasoning`+`text`,签名在 `extra_content.<provider>.thought_signature`),所以出站适配器无需按 provider 分支,只按**客户端来源协议**分支。
- **usage 映射**:Anthropic → `input_tokens`/`output_tokens`;OpenAI 系 → `prompt_tokens`/`completion_tokens`(用 Raycast 的 `input_tokens`/`output_tokens` 换算;`cached_tokens` 可填入 `prompt_tokens_details.cached_tokens`)。

---

## 5. 未决 / 需进一步确认

1. **签名算法** `x-raycast-signature-v2`:本次抓包无法反推。实现时预期复用用户 Raycast token 及其 deviceid;若后端校验签名与 body 绑定,需要进一步逆向。
2. **模型清单**:本包仅见 `claude-haiku-4-5-20251001-reasoning`、`gpt-5.6-luna`。完整可用模型与其 `provider`/是否支持 think,需要抓 Raycast 的模型列表接口(或 `x-raycast-experimental: autoModels` 相关接口)补全。
3. **纯非 think 模型的响应**:本包中非 think 的仅为标题生成(`ai_chat_title`)调用;正式对话里非 think 模型的 `reasoning` 是否恒为空,建议再抓一条确认。
4. **错误响应体结构**:本包无失败样本,后端错误时的 SSE/JSON 结构待补抓。
