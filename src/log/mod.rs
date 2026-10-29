//! 请求日志域:推理请求的入站体、上游请求体与上游响应,存入 SQLite,
//! 按时间保留 3 天自动清理。建在 [`crate::db`] 之上;写入是旁路,
//! 任何失败只记日志,不影响请求处理。
//!
//! [`format`] 服务另一面 —— `tracing` 控制台输出的可读性(碎片 delta
//! 攒行、超长字段截断),与 SQLite 存储互不依赖。

pub mod format;
pub mod store;
pub mod wire;
