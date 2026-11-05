//! Request-correlation support for the request-log store.
//!
//! [`REQUEST_ID`] is a task-local correlation id set by the `debug_capture`
//! middleware; the upstream client and the SSE bridge use it to attribute
//! their log writes to the originating inbound request — without threading an
//! id through every handler and pipeline signature.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

tokio::task_local! {
    /// Correlation id for the request currently being handled. Set by the
    /// `debug_capture` middleware so the Raycast client can name its upstream
    /// dump after the originating inbound one — without threading an id through
    /// every handler and pipeline signature.
    pub static REQUEST_ID: String;
}

/// The current request's correlation id, or `"-"` when called outside a
/// captured request (e.g. the background models fetch).
pub fn current_request_id() -> String {
    REQUEST_ID
        .try_with(|id| id.clone())
        .unwrap_or_else(|_| "-".to_string())
}

/// Parse bytes as JSON for pretty display, falling back to a lossy UTF-8 string
/// value when the body isn't valid JSON. Shared by both capture sites.
pub fn body_to_value(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
}

/// 上游通道登记:request_id → 最终成功送达上游的通道("direct" 或代理 URL)。
/// `try_chat` 成功后写入,中间件的完成日志读取后打印,让 trace 能看出这条
/// 请求走的是直连还是哪个代理。无代理配置时不上报,日志里也不打印。
///
/// 进程内旁路表(与请求日志 store 同级),条目由中间件在请求结束后取走,
/// 只在极端情况(请求被 abort)下残留,量级为单条字符串,不做主动清理。
fn channel_registry() -> &'static Mutex<HashMap<String, String>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 登记某请求实际使用的上游通道。`channel` 为 `"direct"` 或代理 URL。
pub fn record_channel(request_id: &str, channel: &str) {
    if let Ok(mut map) = channel_registry().lock() {
        map.insert(request_id.to_string(), channel.to_string());
    }
}

/// 取走某请求的通道登记(取后即删);未登记(直连且无代理配置)返回 `None`。
pub fn take_channel(request_id: &str) -> Option<String> {
    channel_registry()
        .lock()
        .ok()
        .and_then(|mut map| map.remove(request_id))
}
