# 多模态(图片)支持 — 实施计划

日期:2026-11-21。逆向依据:Raycast Windows 2.13.1.0 客户端 `backend/index.mjs`
(`iln` 上传函数、`w$t` 消息映射、`lOt`/`R$t` 视觉门控、SSE image 事件解析)。

## 背景

客户端(Codex / Claude Code / 任意 OpenAI / Anthropic 兼容客户端)在请求里携带
图片时,网关目前直接丢弃:

- canonical 消息模型是纯文本:`Message.text: String`,无图片槽位;
- 三个入站适配器显式丢图:chat `content_text` 只挑 text part
  (`chat_completions/inbound.rs`),messages `image` block 落入
  `ContentBlock::Other`,`responses` 的 `input_image` 不保留;
- builder 只会发 `{content:{text}}`,没有 attachments 形状。

上游真实协议(逆向确认):图片**先上传、后按 fileId 引用**,从不内联 base64。

## 上游协议(逆向结论)

### 识图(vision)

```
① POST https://backend.raycast.com/api/v1/ai/files
   body:  {"chat_id":"<chatId>","blob":{"filename":"<名>","byte_size":N,
          "content_type":"image/png","checksum":"<md5-base64>"}}
   headers: 与 chat 相同的签名四件套(Content-Type: application/json,
            X-Raycast-Timestamp / X-Raycast-DeviceId / X-Raycast-Signature-v2,
            Authorization: Bearer),UA 用 USER_AGENT_POST
   签名:  calculate_signature_v2(ts, device_id, body_json_string, secret)
   响应:  {"id":"<fileId>","direct_upload":{"url":"<presigned PUT>","headers":{...}}}

② PUT 原始图片字节 → direct_upload.url,头带 direct_upload.headers
   (预签名 URL,通常指向 S3/R2,不走 backend.raycast.com)

③ 聊天请求的用户消息带附件引用(永不内联 base64):
   {"role":"user","content":{"text":"<文本>",
    "attachments":[{"id":"<fileId>","type":"image"}]}}
```

### 视觉门控(纯客户端行为)

- 模型 catalog `abilities.vision.formats = [image/png, image/jpeg, image/webp, image/gif]`;
- 客户端只回放最近 **20 个** 图片附件(`lOt(msgs, 20)`),更老的换成文本占位
  `[image omitted: the selected model has no vision support]`;
- 模型无 vision → 全部附件剥除,文本塞同样的占位符
  (`R$t`,理由 `vision-unsupported`)。

### 生图(image generation)

不是独立端点,同一个 `ai/chat_completions` SSE 流:

- 请求侧:tools 追加 remote tool `{id:"dalle", name:"image_generation"}`(客户端
  preset 的 `image_generation: true` 映射);
- SSE 事件:
  - `notification_type === "generating_image"` → 开始(带 `image_size`
    (默认 "512x512")、`image_count`(默认 1));
  - chunk 顶层字段 `image: "<url>"` → 完成,值为生成图 URL;
  - `notification_type === "tool_failed"` 且 `image_generation: true` → 失败;
  - 错误 code/subcode `image_generation_failed`。

## 网关现状(代码位置)

| 层 | 文件 | 现状 |
|---|---|---|
| canonical 消息 | `src/core/message.rs` | `Message { text, thinking, tool_calls, ... }` 纯文本 |
| canonical 事件 | `src/core/event.rs` | `CanonicalEvent` 无 image 事件 |
| chat 入站 | `src/protocol/chat_completions/inbound.rs` | `content_text()` 过滤非 text part |
| messages 入站 | `src/protocol/messages/wire.rs` | `image` block → `ContentBlock::Other` 丢弃 |
| responses 入站 | `src/protocol/responses/wire.rs` | `input_image` part 无字段、丢弃 |
| builder | `src/raycast/builder.rs` | `RaycastMessage::text()`,固定 `{text}` |
| 上游 wire | `src/raycast/wire.rs` | `RaycastMessageContent { text }` 无 attachments |
| 客户端 | `src/raycast/client.rs` | `chat()` 已有完整签名 POST;无 `ai/files` 上传 |
| 签名 | `src/signature.rs` | `calculate_signature_v2` 直接复用 |
| 模型能力 | 前端 `models.js` 读 `abilities.vision` / `abilities.image_generation` | 数据源已有,网关侧无消费 |

## 实施批次

### 批次 A — 识图主链路(步骤 1-4)

#### A1. canonical 模型加附件(`core/message.rs`)

```rust
/// 一个待上传/已引用的图片附件。canonical 层持有原始字节(解码后的
/// base64 或客户端直传的字节),builder 负责上传并替换成上游 fileId。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub media_type: String,   // "image/png" 等,校验过在 vision.formats 内
    pub data: Vec<u8>,        // 原始字节
}

pub struct Message {
    ...
    /// 用户消息携带的图片附件(仅 Role::User 有意义)。
    pub attachments: Vec<Attachment>,
}
```

`Message::text()` 保持签名不变(内部 `attachments: Vec::new()`),加
`Message::with_attachments(role, text, attachments)` 构造器。

设计决策:
- **canonical 持原始字节**, fileId 是 Raycast 传输细节,不进 canonical;
- 附件只允许出现在 User 消息上(与官方客户端一致;assistant/tool 带附件
  是 chat_id 级文件的引用,网关场景无意义)。

#### A2. 三个入站适配器收图

统一辅助函数(放 `core/message.rs` 或 `util`):解析 OpenAI `image_url` /
Anthropic `image.source` 为 `(media_type, bytes)`:

- `data:<mime>;base64,<payload>` → 直接解;
- `https://...` → 网关拉取(reqwest GET,带上限如 20 MiB,超时复用
  `upstream_timeout`)。拉取失败 → 该附件丢弃并在日志里 warn(不 fail 请求,
  与"日志旁路不影响主链路"的既有原则一致)。

各协议映射:

| 协议 | 入站形状 | 提取 |
|---|---|---|
| chat_completions | `content[].image_url.url`(data URL 或 http) | mime 从 data URL 前缀;http 拉取后按响应 Content-Type 或字节嗅探 |
| messages | `content[]{type:"image", source:{type:"base64", media_type, data}}` 与 `{type:"url", url}` | base64 解或拉取 |
| responses | `content[]{type:"input_image", image_url, detail?}` | 同 chat |

wire 类型各加对应变体(`ContentBlock::Image`、`ContentPartIn` 加 image 字段
等),不再落 `Other`。

#### A3. 上传客户端(`raycast/client.rs` 新方法 + `config.rs` 新 URL)

```rust
pub const RAYCAST_FILES_URL: &str = "https://backend.raycast.com/api/v1/ai/files";

impl RaycastClient {
    /// 上传一个图片附件,返回上游 fileId。走与 chat() 相同的签名/通道/
    /// Attempt 统计。失败返回 Err,由调用方降级。
    pub(crate) async fn upload_file(
        &self, email: &str, token: &str, device_id: &str,
        chat_id: &str, filename: &str, content_type: &str, bytes: &[u8],
        call: &CallContext,
    ) -> Result<String, GatewayError>
}
```

流程:
1. body = `{"chat_id":..., "blob":{"filename":..., "byte_size":..., "content_type":..., "checksum": md5-base64}}`;
2. 签名同 chat(`calculate_signature_v2(ts, device_id, body, secret)`),头同
   `chat()`(含 `X-Raycast-Experimental: autoModels`,UA = USER_AGENT_POST);
3. 响应解析 `{id, direct_upload:{url, headers}}`;
4. `PUT bytes → direct_upload.url`,头 = direct_upload.headers 原样
   (预签名 URL,不叠加后端签名头,与客户端一致);
5. 返回 `id`。

md5 用 `md-5` crate(或手写,选最小依赖)。checksum 是 body JSON 里的
`md5(bytes)` 的 base64 编码。

**失败降级策略**:上传失败(网络/上游 4xx)→ 该附件剥除,文本追加
`[image omitted: upload failed]` 占位符,请求照发。理由:官方客户端对
"模型不支持 vision" 也是剥除+占位,网关照搬该行为比 fail 整个请求更贴近
真实客户端指纹;上传不是请求的关键路径。

#### A4. builder + wire 发附件(`raycast/wire.rs`、`raycast/builder.rs`)

wire 类型:

```rust
pub struct RaycastMessageContent {
    pub text: String,
    /// 有图时出现:上游按 id 解引用。无图请求不加字段(指纹不变)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<RaycastAttachment>>,
}

#[derive(Serialize)]
pub struct RaycastAttachment {
    pub id: String,   // 上传返回的 fileId
    #[serde(rename = "type")]
    pub kind: &'static str,  // 恒 "image"
}
```

关键:**无附件请求的 wire 字节与现在完全一致**(`skip_serializing_if`),
现有指纹对齐不破坏。

builder 流程变为 async(需要上传):
- `build_chat_request` → `async fn build_chat_request(...)`,对每条带
  attachments 的 User 消息逐个上传(`chat_id = thread_id`,同一会话稳定,
  与官方客户端 chat_id 语义一致),拿到 fileId 塞进 `attachments`;
- 上传结果带个小型进程内缓存(key = sha256(bytes + media_type),value =
  fileId):同一张图在一个会话的多轮里只上传一次,与官方客户端 file 复用
  行为一致;缓存有上限(如 256 条,LRU 或直接截断)防膨胀。
- 回放限制:整个请求里只保留最近 20 个带附件的用户消息的附件,更早的
  剥除并替换文本占位(与 `lOt(e.messages, 20)` 一致)。

调用链变化:`client.chat_completions`(dispatcher 侧)在构建 body 前先做
上传;上传用的账号 = 本次请求选中的账号(token/device_id 一起)。

#### A5. 测试

- canonical:附件构造/Default 空;
- 入站:三协议各一个 data-URL/base64 样例 → canonical attachments 断言;
- builder:无图请求序列化字节与旧版本一致(指纹回归测试);有图请求
  (mock 上传)→ `content.attachments` 形状断言;>20 张图回放截断断言;
- client:`ai/files` 请求头/body/签名断言 + presigned PUT 透传断言。

### 批次 B — 门控与生图(步骤 5-6)

#### B1. vision 门控(步骤 5)

- dispatcher/模型解析处读所选模型的 `abilities.vision`(models 缓存里已有
  原始 abilities JSON,`RaycastRawModel.extra`);
- 无 vision:canonical 附件全部剥除,文本追加
  `[image omitted: the selected model has no vision support]`(与客户端
  字符串一致),不发起上传;
- media_type 不在 `vision.formats` 内的同剥除(格式不支持)。

#### B2. 生图(步骤 6)

请求侧:
- 客户端显式请求生图:非标准扩展,暂不做自动注入。生图由模型侧触发
  (官方客户端 preset 才注入 dalle remote tool;网关客户端可用
  `tools: [{type: "function", function: {name: "image_generation"...}}]`
  的 remote 映射?——**不**,local_tool 是网关侧执行的;做法:识别名为
  `image_generation` 的 tool 定义 → builder 转成 remote_tool
  `{id:"dalle", name:"image_generation"}` 注入,模型通过上游侧执行生图,
  SSE 返回结果。客户端拿到的 tool_call 由上游下发,网关透传即可)。

响应侧(parser):
- `RaycastSseData` 加 `#[serde(default)] image: Option<String>`;
- parser 识别 `notification_type == "generating_image"`:
  - 新 CanonicalEvent:`ImageGenerationStarted { size, count }`(可选,先
    只打日志);
  - chunk 带 `image` URL → `CanonicalEvent::ImageGenerationCompleted(url)`;
- 出站适配器:
  - chat:assistant message 里塞 `images: [url]`(OpenAI 无标准形状,选择:
    以 markdown `![image](url)` 进 text delta —— 最通用,客户端都能显示);
  - messages:content block `{type:"image", source:{type:"url", url}}`;
  - responses:`{type:"image_generation_call", result: url}`(或简化为
    output_text 的 markdown,首批从简:三个协议统一走 markdown 进
    TextDelta,复杂形状后续再迭代)。

首批从简决策:**生图结果统一作为 markdown 图片文本下发**,不实现各协议
的原生 image 输出形状;日志侧 `→ image [id]: <url>` 打一条。

## 不做 / 后续

- SVG 附件(客户端显式排除 `sOt(e)` 过滤 SVG)——不支持;
- `ai/files/duplicate`、`DELETE ai/files` 生命周期管理 —— 客户端管自己的
  chat,网关无 chat 生命周期,不调;
- 生图的原生协议形状(chat 的 `images[]` / messages 的 image block 输出 /
  responses 的 `image_generation_call` item)—— 视 B 批落地后客户端反馈;
- Ollama / BYOK 路径 —— 网关不涉及。

## 风险

1. **指纹**:带附件请求是新增 wire 形状,无法对拍真实客户端(本机抓包
   需要真发图)。缓解:形状严格按逆向结论(`{id, type:"image"}` 只此两
   字段);无图请求字节不变。
2. **上传账号一致性**:chat 用哪个账号,上传就用哪个账号(attachment 的
   ACL 绑定账号/chat);dispatcher 已保证单请求单账号,上传在 body 构建
   时同步做,天然一致。
3. **大请求内存**:图片 base64 解码后驻留 canonical;16 MiB body 限制
   兜底,无需新增上限,但拉取 http URL 时单独限 20 MiB。
4. **`mid_conversation` 字段**:客户端带图请求还会带
   `mid_conversation:{system_messages:true, tool_changes:...}`(能力探测
   结果)。网关不加该字段——它不在现有指纹内,先不加,观察上游是否接受。
