//! 控制台日志格式化辅助:碎片 delta 攒行与超长字段截断。
//!
//! 与 [`store`](super::store) 的 SQLite 请求日志不同,这里服务的是
//! `tracing` 控制台输出的可读性:上游 SSE 一个 payload 只有几个字符,
//! 逐条打日志没法观察;thought_signature 之类的密钥串动辄上万字符,
//! 刷屏且无排查价值。两个辅助都只影响日志展示,不影响业务数据。

/// 同类 delta 攒行的上限(字符):攒到这个长度强制刷一行,防止单条日志
/// 无限膨胀(一条思考块可能上万字符)。
const DELTA_FLUSH_CHARS: usize = 4000;

/// 单条日志里超长字段的截断上限(字符)。
const FIELD_MAX_CHARS: usize = 64;

/// 超长字段截断:留前 [`FIELD_MAX_CHARS`] 字符 + `... (N chars)` 标注全长。
pub fn truncate_for_log(s: &str) -> String {
    let n = s.chars().count();
    if n <= FIELD_MAX_CHARS {
        return s.to_string();
    }
    let head: String = s.chars().take(FIELD_MAX_CHARS).collect();
    format!("{head}... ({n} chars)")
}

/// 同类文本 delta 的日志缓冲:`ReasoningDelta` / `TextDelta` 是碎片化的
/// (上游一个 payload 几个字符),逐条打日志没法读。缓冲拼接,在事件类型
/// 切换、终止事件、流结束或超过 [`DELTA_FLUSH_CHARS`] 时整行刷出。
#[derive(Default)]
pub struct DeltaLogBuffer {
    kind: Option<&'static str>,
    buf: String,
}

impl DeltaLogBuffer {
    /// 追加一个 delta;事件类型切换时先刷出前一段。
    pub fn push(&mut self, kind: &'static str, text: &str, id: &str) {
        if self.kind != Some(kind) {
            self.flush(id);
            self.kind = Some(kind);
        }
        self.buf.push_str(text);
        if self.buf.chars().count() >= DELTA_FLUSH_CHARS {
            // 超限强制刷出;kind 一并清空,下一段从新的一行重新开始。
            self.flush(id);
        }
    }

    /// 刷出当前缓冲为一行(换行转义为 `\n` 字面量保证物理单行);空则不动。
    /// 无论刷没刷,kind 都归位 —— 缓冲空时 kind 也必须是无主状态。
    pub fn flush(&mut self, id: &str) {
        if !self.buf.is_empty() {
            let label = self.kind.unwrap_or("text");
            tracing::info!("→ {label} [{id}]: {}", self.buf.escape_debug());
            self.buf.clear();
        }
        self.kind = None;
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
    fn truncate_for_log_keeps_short_and_annotates_long() {
        assert_eq!(truncate_for_log("short"), "short");
        let long = "a".repeat(200);
        let t = truncate_for_log(&long);
        assert!(t.starts_with(&"a".repeat(64)));
        assert!(t.ends_with("... (200 chars)"));
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
