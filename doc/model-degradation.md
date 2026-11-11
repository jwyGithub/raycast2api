# 模型降级机制与断流排查

> 记录 Raycast 上游的模型降级(degradation)机制:什么账号、什么模型会被降级、降到哪、网关如何感知与处理,以及随之而来的上游断流问题。数据来自 2026-11-11 对 55 个模型的全量实测(探测脚本:[`scripts/probe_degrade.py`](../scripts/probe_degrade.py),结果:[`scripts/degrade_report.json`](../scripts/degrade_report.json)),以及单变量隔离测试([`scripts/isolation_test.py`](../scripts/isolation_test.py))。

---

## 1. 结论速览

- **降级根因是客户端 User-Agent 版本,不是账号订阅**。上游按 UA 版本判定 Better AI(Advanced AI)资格:UA 过老(如 `Raycast/1.94.2`)时,所有 Better AI 档模型一律被降级——无论账号是什么订阅状态。UA 为近期真实客户端版本(如 `Raycast/2.11.0.0`)时,Pro trial 账号即可正常使用 Better AI 档模型。
- **降级目标是模型元数据里的 `pro_plan_replacement_model`**,每家降到自己家的替补(见 §3 映射表)。
- **降级连接不稳定**:实测中所有中途断流的请求全部发生在降级链路上。
- 降级通知**不透传给客户端**,只出现在网关日志(store 中的 `response_text`)与终端 WARN 日志中。

---

## 2. 降级通知的形态

上游 SSE 流的**第一个 payload** 携带 `model_updated` 通知:

```json
{"notification_type":"model_updated","notification":"No valid Advanced AI subscription. Using gpt-6-luna instead","text":""}
```

要点:

- HTTP 状态仍是 200,流照常输出 —— 降级是"静默"的,不看日志根本发现不了;
- 通知文本中含实际替补模型名(`Using <model> instead`),是降级判定与目标识别的依据;
- 随后跟着的 `ping` 通知、reasoning/text delta 均为替补模型的真实输出。

## 3. 降级映射表(2026-11-11 实测,旧 UA 1.94.2 下)

55 个模型全量探测:22 个降级、33 个正常,**降级集合与 models 元数据的 `in_better_ai_subscription: true` 完全一一对应**。该表在 UA 修复后仅作历史参照;用新 UA 复测应全部为"未降级"。

替补规则:每家降到**自己家的 `pro_plan_replacement_model`**,不是统一砸到某个兜底模型。

| 降级目标 | 数量 | 被降级的模型 |
| --- | --- | --- |
| `gpt-6-luna` | 9 | gpt-5.4、gpt-5.4-reasoning、gpt-5.5、gpt-5.5-instant、gpt-5.6-terra、gpt-6-sol、gpt-6-astra、gpt-6.1-sol、gpt-6.1-terra |
| `claude-haiku-5-5` | 6 | opus-5、opus-5-5、sonnet-4-6、sonnet-4-6-reasoning、sonnet-5、sonnet-5-5 |
| `gemini-3.8-flash` | 3 | gemini-3.1-pro、gemini-4-argon、gemini-4-carbon |
| `meta/muse-spark-1.3` | 1 | muse-wonder-1.0 |
| (未取到) | 3 | mistral-large、mistral-medium、perplexity-sonar-pro(该三条探测时即断流,通知未落到日志;按映射规律应分别为 mistral-small-latest 系与 sonar 系替补) |

未降级(33 个):haiku 全系、gpt-5.4-mini、gpt-6-luna(降级目标本体)、全部 `gateway-*` 转发模型、gemini flash 系、mistral-small/nemo/codestral、groq gpt-oss 系、raycast-ray1 系、grok 系等。

## 4. 根因定位:User-Agent 版本判定(2026-11-11 定案)

### 4.1 排除账号因素的过程

按假设强度依次排除:

1. **账号订阅快照过期** → 对 3 个真实账号 `/v1/accounts/refresh` 前后测试,全部照样降级(排除);
2. **账号注册新鲜度 / credits 消耗** → refresh 后账号 `pro/trialing`、`trial_days_left: 5`、credits 480/500,状态健康依旧降级(排除);
3. **A/B 实验(决定性)**:把官方客户端抓包中的账号(客户端内实测不降级)原样导入独立实验室网关(空池、同 token、同签名)→ **走网关即降级**。同一账号两条路径结果不同,差异必然在请求侧。

### 4.2 单变量隔离测试

从抓包与网关日志对比请求差异,锁定 UA 后做隔离实验(`scripts/isolation_test.py`):手工签名直打上游,请求体完全相同,仅 UA 不同:

| User-Agent | gpt-6.1-sol 结果 |
| --- | --- |
| `Raycast/1.94.2 (macOS Version 15.3.2 (Build 24D81))`(旧网关 UA) | 降级 → gpt-6-luna |
| `Raycast/2.11.0.0 (x-Windows-amd64 10.0.26100.9711)`(真实客户端) | **未降级**,3/3 稳定 |

### 4.3 修复与验证

- `src/config.rs` 的 `USER_AGENT` 更新为真实客户端版本;注册脚本(`raycast_register.mjs` / `raycast_auto_register.mjs`)的 2.3.1.0 一并对齐。
- 实验室网关(独立实例,池中仅抓包账号)端到端复测:gpt-6.1-sol 正常完成,上游流中无降级通知。

### 4.4 维护提示

- 上游 UA 判定是**版本门槛**而非精确匹配:未来客户端升级后,网关 UA 需跟进抓包更新;
- 排查"整批模型突然集体降级"时,优先怀疑 UA 过老/上游调整了版本门槛,其次才是账号侧。

## 5. 断流问题与网关处理

### 5.1 现象

降级链路上的长 reasoning 流会被上游/代理**静默掐断**:

- reqwest 报 `error decoding response body`(chunked 响应体中途截断);
- 无 in-stream error payload、无 `[DONE]`、无 `finish_reason`;
- 掐断点集中在 reasoning 中段(实测 100+ payload 后)。

实测相关性:3 条未正常结束的探测请求**全部**是降级模型;33 个未降级模型全部正常 finish。

### 5.2 网关的感知与处理(`src/raycast/stream.rs`)

1. **传输错误留痕**:`bytes_stream()` 报错时打 `ERROR ⇈ upstream transport error [id]`,带具体错误与已收 payload 数(此前为静默 break,排查时无迹可循);
2. **断流合成错误**:流结束但从未出现 Finish/Error 事件时,打 `ERROR ⇈ upstream stream ended without finish`,并合成 `CanonicalEvent::Error { status: 502 }` 下发;
3. **通知上终端**:SSE 通知增量以 WARN 打印:`⇈ upstream notification [id]: No valid Advanced AI subscription...`。

### 5.3 客户端可见性

合成错误事件后,三个协议端点都会把中断原因如实推给客户端,不再谎报完成:

| 端点 | 断流时客户端收到 |
| --- | --- |
| `POST /v1/responses` | `response.failed` 帧,`error.message` 带原因 |
| `POST /v1/messages` | SSE `error` 事件帧 |
| `POST /v1/chat/completions` | `data: {"error": {...}}` 帧 |

(修复前的行为:断流被当作正常结束,Responses 端发出 `response.completed`,客户端拿到半截内容且空 usage,连重试都不会触发。)

## 6. 如何复测

```bash
# 在 scripts/ 下执行;需要网关 key 与 /v1/logs 权限
uv run --no-project python -I probe_degrade.py
```

- 脚本逐个模型发最小非流式请求,经 `/v1/logs/list` → `/logs/detail` 关联 request_id,从上游原始流中检索降级标记 `No valid Advanced AI subscription`,并正则提取替补模型写入 `degraded_to` 字段;
- 换账号池/换 UA 后直接重跑即可刷新 `scripts/degrade_report.json`;
- 快速单点验证某模型是否降级:发一个请求后在终端看 `⇈ upstream notification` WARN 行,或查 `/v1/logs/detail` 的 `response_text`;
- 单变量隔离测试(UA 直打上游):`scripts/isolation_test.py`(内含 rot13_rot5 + HMAC-SHA256 签名的 Python 移植)。
