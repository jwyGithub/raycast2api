//! src/log/wire.rs
//! 请求日志的对外 JSON 形状:列表项(不含 body)与详情(三体全文)。

use serde::Serialize;

/// `GET /v1/logs` 的一条列表项 —— 不含 body,只有元信息与各体大小。
#[derive(Debug, Serialize)]
pub struct LogEntry {
    pub id: i64,
    pub request_id: String,
    pub created_at: i64,
    pub method: Option<String>,
    pub path: Option<String>,
    pub inbound_bytes: Option<i64>,
    pub upstream_bytes: Option<i64>,
    pub response_bytes: Option<i64>,
}

/// `GET /v1/logs/{request_id}` 的详情 —— 三体全文。
#[derive(Debug, Serialize)]
pub struct LogDetail {
    pub request_id: String,
    pub created_at: i64,
    pub method: Option<String>,
    pub path: Option<String>,
    pub inbound_json: Option<String>,
    pub upstream_json: Option<String>,
    pub response_text: Option<String>,
}
