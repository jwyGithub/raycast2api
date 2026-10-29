//! Bridges the Raycast SSE transport to the canonical event stream.
//!
//! [`canonical_events`] owns the upstream [`reqwest::Response`] and yields a
//! provider-agnostic [`CanonicalEvent`] stream: it feeds the raw byte chunks
//! through the [`SseDecoder`], then maps each decoded `data:` payload with the
//! [`parser`](super::parser). This is the single place the byte transport meets
//! component ⑦, so every consumer — the streaming outbound adapters and the
//! non-streaming aggregation alike — observes exactly the same event sequence.
//!
//! A mid-stream transport error simply ends the stream, keeping the events
//! decoded so far. This matches the previous inline handler: the streaming path
//! stops emitting, and the non-streaming path returns whatever it aggregated.

use async_stream::stream;
use futures_util::{Stream, StreamExt};

use crate::core::event::CanonicalEvent;
use crate::debug;
use crate::log::store::RequestLogStore;

use super::parser::parse_events;
use super::sse::SseDecoder;

use std::sync::Arc;

/// 日志缓冲上限(字符):同类 delta 攒到这个长度强制刷一行,防止单条日志
/// 无限膨胀(一条思考块可能上万字符)。
const LOG_FLUSH_CHARS: usize = 4000;

/// 同类文本 delta 的日志缓冲:`ReasoningDelta` / `TextDelta` 是碎片化的
/// (上游一个 payload 几个字符),逐条打日志没法读。缓冲拼接,在事件类型
/// 切换、终止事件、流结束或超过 [`LOG_FLUSH_CHARS`] 时整行刷出。
#[derive(Default)]
struct DeltaLogBuffer {
    kind: Option<&'static str>,
    buf: String,
}

impl DeltaLogBuffer {
    /// 追加一个 delta;事件类型切换时先刷出前一段。
    fn push(&mut self, kind: &'static str, text: &str, id: &str) {
        if self.kind != Some(kind) {
            self.flush(id);
            self.kind = Some(kind);
        }
        self.buf.push_str(text);
        if self.buf.chars().count() >= LOG_FLUSH_CHARS {
            // 超限强制刷出;kind 一并清空,下一段从新的一行重新开始。
            self.flush(id);
        }
    }

    /// 刷出当前缓冲为一行(换行转义为 `\n` 字面量保证物理单行);空则不动。
    /// 无论刷没刷,kind 都归位 —— 缓冲空时 kind 也必须是无主状态。
    fn flush(&mut self, id: &str) {
        if !self.buf.is_empty() {
            let label = self.kind.unwrap_or("text");
            tracing::info!("→ {label} [{id}]: {}", self.buf.escape_debug());
            self.buf.clear();
        }
        self.kind = None;
    }
}

/// Decode a Raycast chat SSE response into a stream of [`CanonicalEvent`]s.
pub fn canonical_events(
    response: reqwest::Response,
    log: Arc<RequestLogStore>,
) -> impl Stream<Item = CanonicalEvent> {
    // Grab the correlation id *here*: the task-local REQUEST_ID scope is only
    // valid inside the handler, while the stream below is polled outside it.
    // The stream! block is an async move, so both are moved into it.
    let id = debug::current_request_id();

    stream! {
        let mut payloads: Vec<String> = Vec::new();
        let mut bytes = response.bytes_stream();
        let mut decoder = SseDecoder::new();
        let mut text_log = DeltaLogBuffer::default();

        while let Some(chunk) = bytes.next().await {
            let Ok(chunk) = chunk else { break };
            for payload in decoder.push_bytes(&chunk) {
                payloads.push(payload.clone());
                // ponytail: 打印原始 SSE 数据用于调试
                tracing::debug!("upstream SSE payload: {}", payload);

                for event in parse_events(&payload) {
                    // The outbound adapters stop polling the stream at `Finish`,
                    // so this generator's tail code never runs on the streaming
                    // path — record as soon as the terminating payload is seen.
                    // Anything after it is `[DONE]` / notifications.
                    if matches!(
                        event,
                        CanonicalEvent::Finish { .. } | CanonicalEvent::Error { .. }
                    ) {
                        log.update_response(&id, &payloads.join("\n"));
                    }
                    // 打印解析后的规范事件:碎片 delta 攒行,其余逐条。
                    match &event {
                        CanonicalEvent::TextDelta(text) => text_log.push("text", text, &id),
                        CanonicalEvent::ReasoningDelta(text) => {
                            text_log.push("reasoning", text, &id)
                        }
                        CanonicalEvent::ThoughtSignature(sig) => {
                            text_log.flush(&id);
                            tracing::info!("→ thought_signature [{id}]: {}", sig);
                        }
                        CanonicalEvent::ToolCall(call) => {
                            text_log.flush(&id);
                            tracing::info!("→ tool_call [{id}]: {} (id: {})", call.name, call.id);
                        }
                        CanonicalEvent::Finish { reason, usage } => {
                            text_log.flush(&id);
                            tracing::info!("→ finish [{id}]: reason={:?}, tokens={{input: {}, output: {}, cached: {}}}",
                                reason, usage.input_tokens, usage.output_tokens, usage.cached_tokens);
                        }
                        CanonicalEvent::Error { status, message } => {
                            text_log.flush(&id);
                            tracing::error!("→ upstream error [{id}]: status={status}, message={message}");
                        }
                    }
                    yield event;
                }
            }
        }

        // 流结束:刷出残留的缓冲。
        text_log.flush(&id);

        // One raw `data:` payload per line, exactly as the upstream sent it
        // (non-streaming aggregation path polls the stream to completion).
        log.update_response(&id, &payloads.join("\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_buffer_accumulates_same_kind_and_flushes_on_switch() {
        let mut buf = DeltaLogBuffer::default();
        buf.push("reasoning", "He", "r1");
        buf.push("reasoning", "llo", "r1");
        // 未切换/未超限时不出日志(无副作用可断言,靠下面 flush 的输出验证)。
        assert_eq!(buf.buf, "Hello");
        assert_eq!(buf.kind, Some("reasoning"));

        // 切换类型 → 先刷出 reasoning,再开始攒 text。
        buf.push("text", "wo", "r1");
        assert_eq!(buf.buf, "wo");
        assert_eq!(buf.kind, Some("text"));
    }

    #[test]
    fn delta_buffer_flushes_at_char_limit() {
        let mut buf = DeltaLogBuffer::default();
        for _ in 0..600 {
            buf.push("text", "12345678", "r1"); // 4800 字符 > 4000 上限
        }
        // 超限后 flush,残留 800 字符继续攒下一段(类型不变,接着攒)。
        assert_eq!(buf.buf.chars().count(), 800);
        assert_eq!(buf.kind, Some("text"));

        // 显式 flush 后缓冲清空。
        buf.flush("r1");
        assert_eq!(buf.buf, "");
        assert_eq!(buf.kind, None);
    }

    #[test]
    fn delta_buffer_flush_empty_is_noop() {
        let mut buf = DeltaLogBuffer::default();
        buf.flush("r1"); // 空缓冲刷出不应 panic 也不留 kind
        assert_eq!(buf.kind, None);
        buf.push("text", "x", "r1");
        buf.flush("r1");
        assert_eq!(buf.buf, "");
    }
}
