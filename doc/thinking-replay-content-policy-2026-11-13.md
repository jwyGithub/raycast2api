# 历史 thinking 回放触发上游安全拦截:分析与修复

> 记录 raycast2api 网关 2026-11-12/13 连续两波 `content_policy_blocked`(400)的完整排查、根因定位、多方实证(抓包 + 官方文档 + Claude Code harness 逆向 + Codex harness 逆向),以及最终落地的网关侧修复。
> 关联抓包:`sonnet5-5.chlz`(触发样本)、`sonnet5-5-11-13.chlz`、`sonnet5-5-3.chlz`(sonnet 多轮行为实证)、`gpt-6-1-sol-3.chlz`(gpt 多轮行为与跨模型对比)。

---

## 1. 结论速览

- **根因**:上游(Anthropic 经 Raycast 中转)会扫描请求里的**全部消息文本**,其中包括网关回放的历史 `<think>...</think>` 段。这些是模型自己生成的思考文字,概率性撞上内容分类器,整请求 400(`content_policy_blocked` / `refusal_blocked`,`retryable:false`)。
- **触发特性**:不是关键词匹配,是概率性分类器。同一段文本可复现,相邻文本(如 "a banana..." vs "the banana...")结果随机;换账号、重试均无效。
- **第一波(11-12)误判**:当时定位到的是用户 CLAUDE.md 里的"展示思维链"类指令。第二波(11-13)该指令已删、仍被拦,二分重放证明真正的触发面是 assistant 历史 thinking 块本身。
- **官方客户端行为**(抓包实证):Raycast 客户端在多轮对话中**从不回放历史 thinking**——每轮响应里模型明明输出了完整 thinking(数百个 reasoning 事件),下一轮请求的 assistant 历史消息只剩答案正文,`<think>` 与 `thought_signature` 一概不传。该行为对 **sonnet 与 gpt 两模型族一致**(§3.4)。
- **修复**:`src/raycast/builder.rs` 的 `build_messages` 只对**当前 tool-use 循环**(最后一个 user 消息之后)的 assistant 消息回放 `<think>` 与 signature,历史轮全部剥离。等价于官方 `clear_thinking` 的 `keep:N` 语义,且比官方客户端(全剥)更保守。

---

## 2. 排查过程(2026-11-13 第二波)

### 2.1 现象

客户端(sonnet-5-5,xhigh,102 条消息,452KB upstream 载荷)连续 400:

```
upstream in-stream error status=400 Your request was blocked by the AI provider's safety policy.
```

网关日志自带的 hint 指向"提示词要求展示思维链"——但该指令(第一波的根因)已从 CLAUDE.md 删除,提示不成立。

### 2.2 方法:日志 API + 二分重放

1. `POST /v1/logs/detail`(参数 `request_id`)拿到完整 `inbound_json` / `upstream_json` / `response_text`。
2. 用 inbound(Anthropic 格式)原样重放网关 `/v1/messages` → 400 复现。
3. 对消息数组做二分 + leave-one-out,收敛到 msg0 + msg48 + msg51 组合;再对 msg48 的 thinking 块文本逐句二分。

### 2.3 决定性实验

| 实验 | 结果 |
| --- | --- |
| 完整 payload 重放 | 400 复现 |
| 剥掉全部 **26 个** thinking 块,其余零改动 | **通过** |
| 只删 msg48 那一个 thinking 块 | 通过 |
| 只保留 msg48 的 thinking、删其余 25 个 | 400 |
| 该 thinking 文本单独发(无 system/上下文) | 400 |
| 该文本换成无害文本 | 通过 |

触发源即 msg48 里 598 字符的 assistant thinking 文本("I need to check how Chat Completions handles reasoning... so I'll rerun it...")。

### 2.4 概率性分类器的证据

对固定上下文插入不同 thinking 文本:

- `the banana before writing the README.` → 拦
- `a banana before writing the README.` → 放行
- `something before writing the README.` → 放行
- `apple before writing the README.` → 拦

同一句式结果随机、仅对固定文本可复现 ⇒ 边界是概率性分类器,无法靠改措辞规避,客户端侧无法预防(内容是模型自己生成的)。

---

## 3. Raycast 后端现状(三方实证)

### 3.1 真实客户端抓包(sonnet5-5-3.chlz,同 thread 三轮)

| 请求 | messages | 历史 assistant 内容 | 含 `<think>`? | 含 `thought_signature`? |
| --- | --- | --- | --- | --- |
| 第 1 轮 | 1 | — | — | — |
| 第 2 轮 | 3 | 第 1 轮答案(纯 text) | 否 | 否 |
| 第 3 轮 | 5 | 前两轮问答(纯 text) | 否 | 否 |

每一轮的 **SSE 响应都带 thinking**(第 2 轮 95 个、第 3 轮 750 个 reasoning 事件),但下一轮请求全部丢弃。历史 assistant 消息的 `content` 只有 `text` 一个键。⇒ **上游不依赖历史 thinking 回放维持对话连贯,signature 回传校验在 Raycast 链路上未启用。**

### 3.2 Anthropic 官方语义(文档核对)

- `context_management.edits[].type: "clear_thinking_*"` 的 `keep` 计数的是 assistant turns;历史轮 thinking 被 server-side 替换为 placeholder,**没有签名约束、可安全剥离**。只有**当前 tool-use 循环内**的 thinking+signature 必须原样保留(signature 校验、首块位置)。
- 手动剥历史 thinking 的唯一风险是弄断 `tool_use → tool_result` 配对——网关的 `<think>` 是独立插入的消息,不触碰 content 数组,天然安全。
- "历史 thinking 被内容过滤器重扫"**不是 Anthropic 直连行为**,是 Raycast 中转层的实现细节;官方 `clear_thinking` 的动机只是省 token。

### 3.3 Claude Code harness 逆向(claude.exe 2.1.322)

```js
function Rcn(e) {
  let {hasThinking: n = false, toolClearing: r} = e ?? {}, s = [];
  if (n) s.push({type: "clear_thinking_20251015", keep: "all"});  // 唯一的 thinking 编辑
  if (r) s.push(r);                                               // clear_tool_uses_20250919
  return s.length > 0 ? {edits: s} : void 0
}
```

- harness 发的 `clear_thinking_20251015` **永远是 `keep:"all"`**(硬编码),条件只有 `hasThinking`(本请求开了 thinking)。
- `keep:"all"` 按 API 语义就是 no-op 占位——harness 从不请求清除 thinking;真正的历史清理靠 `clear_tool_uses_20250919`(针对 tool_result)。
- 历史里哪些 thinking 保留、哪些剥掉,由 harness 构造 `messages[]` 时自行决定,与 `context_management` 无关。

**三方结论一致:历史 thinking 不需要、也不应该回放给上游。**

### 3.4 跨模型对比:gpt-6.1-sol vs sonnet-5-5(gpt-6-1-sol-3.chlz,gpt 侧同 thread 三轮)

gpt-6.1-sol 三轮抓包(第 1/2/3 轮 messages = 1/3/5)与 sonnet 结论一致的部分:

- **多轮一律不回放**:第 2、3 轮请求的历史 assistant 消息同样只有答案正文,无 `<think>`、无 `thought_signature`。⇒ "历史不回放"是**全模型统一的不变量**,与模型家族无关。
- 上游 SSE wire 格式统一为 `{reasoning, text, extra_content}`,reasoning 走 `reasoning` 字段、签名走 `extra_content.<provider>.thought_signature`,只是命名空间不同。

差异部分(集中在**响应侧**,不影响请求侧修复):

| | sonnet-5-5 | gpt-6.1-sol |
| --- | --- | --- |
| 签名命名空间 | `extra_content.anthropic.thought_signature` | `extra_content.openai.thought_signature` |
| 每响应签名数 | **1 个**(reasoning 全部结束后、正文前) | **最多 4 个**(round1:reasoning 段间 1 + 连续 3;round3:**流开头密集 4 个**) |
| reasoning 流式形态 | 一次连续 run → 签名 → 正文 | 可分段(run → 签名 → run → 签名×3 → 正文);甚至**无可见 reasoning 只有签名**(round2 全程 1 个空 reasoning 事件 + 1 签名) |

gpt round3 的形态说明 OpenAI 侧签名是"每个推理块一个",推理块内容不可见时签名照发。此差异对网关的一个含义:`ThoughtSignature` 事件目前只保留最后一个签名(单值语义)——sonnet 无损失,gpt 多签名会丢弃前几个。当前循环回放挂的是最后一个签名,与"终结消息携带最新签名"的方向兼容,但 gpt 多签名经网关往返的完整性**尚未经网关侧抓包验证**,上线后建议用 gpt-6.1-sol 跑一轮多 tool-call 会话观察。

另:签名本体是 base64 密文,分类器扫的是 reasoning/thinking **文本**;签名不构成拦截面。

---

## 4. 修复实现

### 4.1 位置与逻辑

`src/raycast/builder.rs` → `build_messages`:

```rust
// Everything from the last user message onward is the live tool-use loop;
// the request is built to continue it, so its thinking stays.
let current_loop_start = messages
    .iter()
    .rposition(|m| m.role == Role::User)
    .unwrap_or(0);
```

- `<think>` 回放与 `thought_signature` 附加都 gate 在 `index >= current_loop_start` 上。
- **保留当前循环**的理由:当前 tool-use 循环的 thinking+signature 是协议要求保留的部分(官方语义),网关此前行为不变,属保守余量。
- **剥离历史轮**的理由:官方客户端抓包证明其无价值(§3.1),且正是 400 的触发面(§2.3)。
- 网关转发后的请求,历史部分与真实 Raycast 客户端逐字节等价;当前循环比官方多保留 thinking+signature,与既有协议实现(builder.rs 既有注释,doc/raycast-backend-protocol.md §2.2)一致。

### 4.2 不解析 `context_management` 的原因

harness 实测只发 `keep:"all"`(no-op),解析它拿不到任何有效指令;官方 `keep:N` 语义(N = 保留最近 N 个 assistant turns)在网关侧的直接等价物就是"最后一个 user 消息之后保留"——用消息数组位置实现,零配置、对所有客户端生效。

### 4.3 `/v1/responses` 端点的覆盖情况(线上日志核实)

三个入站端点(`/v1/chat/completions`、`/v1/messages`、`/v1/responses`)在 [pipeline.rs](../src/server/pipeline.rs) 汇合到同一个 `build_chat_request` → `build_messages`,修复一处三端点同时生效。

对 `/v1/responses` 的入站核实(2026-11-13,线上日志 API 抽样 15 条 / 880 条 responses 请求,覆盖 `gpt-6.1-sol`、`gpt-6-astra`、gemini 等模型):

- 客户端(Codex,`client_metadata.source: "codex"`)每条请求都带 `include: ["reasoning.encrypted_content"]` + `store: false` + `reasoning: {context: "all_turns"}`,语义是"服务端把每轮 reasoning 加密内容发我、我自己存、下轮回传"。
- **但抽样中没有任何一条的 `input[]` 含 `type:"reasoning"` item**(包括 939 items 的超大请求);`encrypted_content` 只出现在 `include` 声明里,从未出现在历史 item 中。即:当前 Codex 客户端**声明了回传协议但实际未回传**。
- 即使未来客户端开始回传,网关 [responses/inbound.rs](../src/protocol/responses/inbound.rs) 的映射器只处理 message/tool 类型 item,`type:"reasoning"` item 在入站适配层直接丢弃——**三层保险**(客户端未回传 → 入站适配丢弃 → builder 剥离),历史 reasoning 到不了 Raycast 出站载荷。

`/v1/responses` 出站响应侧不受影响:模型新输出的 reasoning 仍照常流回客户端(`ReasoningDelta` / `ThoughtSignature` 事件链路未动)。

### 4.3.1 Codex harness 的历史 reasoning 逻辑(逆向核实)

对 Codex 客户端(openai.chatgpt 扩展 26.1106.51428 自带的 `codex.exe`)做了三源交叉验证:二进制逆向、本地 rollout 会话文件、网关日志,结论是 **Codex 存了 reasoning 但从不再发,连当前 tool-use 循环内都不回传**。

**三源证据**(同一 Codex 会话,请求 `01M77AX1VSCHX1C8H06GMQBGXH`,模型 `gpt-6.1-sol`):

| 位置 | reasoning 情况 |
| --- | --- |
| Codex 本地 rollout(`~/.codex/sessions/**/rollout-*.jsonl`) | 每个 tool 循环都完整存储 reasoning item:`{id, summary[], content, encrypted_content, internal_chat_message_metadata_passthrough}` |
| 网关收到的 `/v1/responses` inbound | **0 个 reasoning item**。即使是同一 turn 的 tool 循环内(items 10→28 连续 9 个 `custom_tool_call`/`output`,中间无任何 reasoning item) |
| 网关发往 Raycast 的 upstream | 0 条 `<think>` 消息 |

要点:

- Codex 的 `encrypted_content` 在其**自己的存储里都是 `null`**(响应没携带或未存)。每条请求带的 `include: ["reasoning.encrypted_content"]` 是请求**未来**响应携带加密 reasoning,不是回放历史——`store:false` + 自己存 + 回传的模式下,它实际选择了存 summary、不回传。
- 二进制中 `ResponseItem::Reasoning`(5 字段)只出现在序列化/存储相关代码,未见任何"下一轮请求重建 input"时保留 reasoning 的路径;`GuardianV2TranscriptSource{tool_calls, tool_outputs, reasoning}` 的三元分组也说明 reasoning 在其会话模型里是与工具调用并列的**历史记录**,而非请求重放载荷。

**与网关修复的对照**:

| | Codex harness | Raycast 官方客户端(§3.1 抓包) | 网关(修复后) |
| --- | --- | --- | --- |
| 历史 reasoning/thinking | 不回传 | 不回传 | 不回传(剥到最后一个 user 之前) |
| 当前 tool-use 循环内 | **不回传** | 不适用(单轮无状态) | **回传** `<think>` + signature |

即网关比两个官方客户端都保守,唯一多发的是**当前循环内**的 thinking。这是 Anthropic 直连协议要求保留的部分;但 Codex 实测(同轮循环零回传、多轮工具调用照常成功)+ Raycast 客户端抓包(signature 从不回传、三轮全成功)证明 **Raycast 后端链路不校验 signature、不依赖 reasoning 回放**。

**后续退路**(若上线后仍出现零星 400):把 `build_messages` 里的 `index >= current_loop_start` 条件去掉即全剥,与官方客户端完全对齐,分类器暴露面归零。风险仅是当前轮 thinking signature 不再回传——已有证据表明无影响。当前版本(保留当前循环)作为第一档上线,历史剥离已消除实测的全部触发面;当前循环 thinking 每次请求都重新生成、文本较短,撞分类器概率远低于长期堆积的历史。

### 4.4 回归测试

`tests/request_building.rs` 新增 3 例:

- `historical_thinking_is_not_replayed` — 旧轮 `<think>` 不出现在 wire
- `current_loop_thinking_is_replayed` — 当前循环 `<think>` 照常回放
- `historical_signature_is_dropped_with_its_thinking` — 旧 signature 不回传、当前循环终结消息 signature 保留

全套 220 测试通过,clippy 干净,release 构建成功。

---

## 5. 运维备忘

- **识别**:日志中 `content_policy_blocked` / `refusal_blocked` + `retryable:false`;换账号重试无效即为内容类拦截,先用日志 API 重放二分,别怀疑账号池/签名/模型映射。
- **排查入口**:`POST /v1/logs/list`、`POST /v1/logs/detail`(参数 `request_id`)拿三体全文。
- **局限**:当前循环内的 thinking 仍会上游(协议要求),理论上仍可能撞分类器;若未来复发,可进一步对齐官方客户端(连当前轮也剥,抓包显示 Raycast 后端不校验 signature),或对 thinking 文本做摘要化。
