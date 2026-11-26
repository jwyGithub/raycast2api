//! 统一 trace 格式:`[request_id] - [module] - message`。
//!
//! 所有请求路径的日志经此模块输出,保证控制台按 request_id 串起完整链路:
//!
//! ```text
//! [01J...ABC] - [inference] - model=claude-x upstream=anthropic/claude messages=3 stream=true
//! [01J...ABC] - [dispatcher] - using account a@x via direct
//! [01J...ABC] - [geo] - egress resolved locale=zh-CN tz=Asia/Shanghai (cache)
//! [01J...ABC] - [upstream] - chat sent status=200 elapsed=320ms
//! [01J...ABC] - [inference] - completed status=200 elapsed=412ms
//! ```
//!
//! request_id 由中间件的 [`REQUEST_ID`](super::context::REQUEST_ID) task-local
//! 提供;后台任务(额度刷新、模型缓存等)不在请求作用域内,统一显示 `-`。
//! `request_id` / `module` 同时以结构化 tracing 字段输出,JSON subscriber 可
//! 直接按字段过滤。

use super::context::current_request_id;
use std::sync::OnceLock;

/// 后台任务(request_id 为 `-`)的统一模块标签,便于一眼区分非请求日志。
pub const BACKGROUND_REQUEST_ID: &str = "-";

/// 已警告过的"构造缺陷"类一次性日志键(全局只发一次)。
static WARNED_KEYS: OnceLock<std::sync::Mutex<std::collections::HashSet<&'static str>>> =
    OnceLock::new();

/// 当前请求是否在请求作用域内(供调用点区分后台/请求路径)。
pub fn in_request_scope() -> bool {
    current_request_id() != BACKGROUND_REQUEST_ID
}

/// 一次性警告(每个 key 全局只发一次),用于构造缺陷类提示。
pub fn warn_once(key: &'static str, message: &str) {
    let keys = WARNED_KEYS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut guard = match keys.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !guard.insert(key) {
        return;
    }
    drop(guard);
    crate::trace_warn!("config", "{}: {}", key, message);
}

/// 统一格式的 info 日志:`trace_info!("dispatcher", "using account {}", email)`。
#[macro_export]
macro_rules! trace_info {
    ($module:expr, $($args:tt)*) => {
        $crate::emit_inner!(@info $module, $($args)*)
    };
}

/// 统一格式的 warn 日志。
#[macro_export]
macro_rules! trace_warn {
    ($module:expr, $($args:tt)*) => {
        $crate::emit_inner!(@warn $module, $($args)*)
    };
}

/// 统一格式的 error 日志。
#[macro_export]
macro_rules! trace_error {
    ($module:expr, $($args:tt)*) => {
        $crate::emit_inner!(@error $module, $($args)*)
    };
}

/// 统一格式的 debug 日志(细粒度流程,默认过滤级别下不显示)。
#[macro_export]
macro_rules! trace_debug {
    ($module:expr, $($args:tt)*) => {
        $crate::emit_inner!(@debug $module, $($args)*)
    };
}

/// 统一格式的 trace 日志(最细粒度,需要 RUST_LOG=trace 才显示)。
#[macro_export]
macro_rules! trace_trace {
    ($module:expr, $($args:tt)*) => {
        $crate::emit_inner!(@trace $module, $($args)*)
    };
}

/// 统一格式化并输出。宏规则无法按 level 分发 `tracing::$level!`,
/// 所以直接在展开处 match 一次。
#[doc(hidden)]
#[macro_export]
macro_rules! emit_inner {
    (@$level:ident $module:expr, $($args:tt)*) => {{
        let request_id = $crate::log::context::current_request_id();
        let module: &'static str = $module;
        let message = format!($($args)*);
        tracing::$level!(
            request_id = %request_id,
            module = module,
            "[{}] - [{}] - {}",
            request_id,
            module,
            message
        );
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_id_is_dash() {
        // 不在请求作用域内 → current_request_id 返回 "-"。
        assert_eq!(current_request_id(), BACKGROUND_REQUEST_ID);
        assert!(!in_request_scope());
    }
}

#[cfg(test)]
mod macro_tests {
    /// 编译期验证宏在 crate 内可用 + 消息格式正确。
    #[test]
    fn macros_compile_and_format() {
        // 后台作用域:不该 panic,只发日志。
        crate::trace_info!("trace-test", "hello {} = {}", 1 + 1, "world");
        crate::trace_warn!("trace-test", "warning {}", 42);
    }
}
