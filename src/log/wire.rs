//! src/log/wire.rs
//! 请求日志的对外 JSON 形状:列表元信息与日志详情。

use serde::Serialize;

/// `POST /v1/logs/list` 的一条列表项 —— 不含 body,只有元信息与各体大小。
#[derive(Debug, Serialize)]
pub struct LogEntry {
    pub id: i64,
    pub request_id: String,
    pub created_at: i64,
    pub method: Option<String>,
    pub path: Option<String>,
    pub model: Option<String>,
    /// 最终上游请求体中的模型名称（已应用模型映射）。
    pub upstream_model: Option<String>,
    /// 形如 "请求effort(实际发往上游的effort)"。
    pub effort: Option<String>,
    pub inbound_bytes: Option<i64>,
    pub upstream_bytes: Option<i64>,
    pub response_bytes: Option<i64>,
}

/// `POST /v1/logs/detail` 的详情,包含头部原值与正文。
#[derive(Debug, Serialize)]
pub struct LogDetail {
    pub request_id: String,
    pub created_at: i64,
    pub method: Option<String>,
    pub path: Option<String>,
    pub model: Option<String>,
    /// 最终上游请求体中的模型名称（已应用模型映射）。
    pub upstream_model: Option<String>,
    /// 形如 "请求effort(实际发往上游的effort)"。
    pub effort: Option<String>,
    /// 请求头与响应头均为 `name: value` 逐行文本:头名小写、按名称字典序排列(非线上顺序),
    /// 重复头各占一行,非 UTF-8 值按有损方式解码。值为原值,**包含 `Authorization`、`x-api-key`、
    /// `Cookie`、`Set-Cookie` 等凭证,保留 7 天**,按可读取凭证管理。
    /// 旧记录、尚未发出或尚未收到时为 `null`,不会返回空串。
    pub inbound_headers: Option<String>,
    pub inbound_json: Option<String>,
    /// 仅保留同一 `request_id` 最后一次上游尝试的请求(重试、压缩摘要会覆盖前一次)。
    pub upstream_headers: Option<String>,
    pub upstream_json: Option<String>,
    /// 响应侧在新响应到达时才覆盖,新一轮尝试尚未收到响应时仍是上一轮的内容。
    pub response_headers: Option<String>,
    pub response_text: Option<String>,
}
