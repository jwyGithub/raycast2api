//! 请求日志域:推理请求的入站请求、上游请求与上游响应的头部和正文存入 SQLite,
//! 并在 7 天后自动清理。写入是旁路,
//! 任何失败只记日志,不影响请求处理。
//!
//! [`format`] 服务另一面 —— `tracing` 控制台输出的可读性(碎片 delta
//! 攒行、超长字段截断),与 SQLite 存储互不依赖。

pub mod format;
pub mod store;
pub mod wire;

pub mod context;
