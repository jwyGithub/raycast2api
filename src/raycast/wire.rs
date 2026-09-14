//! Wire types for the Raycast backend — the exact JSON shapes sent to, and
//! received from, `POST /api/v1/ai/chat_completions`.
//!
//! Field shapes are confirmed against real captured traffic in
//! `charles/think_tools.har` and documented in
//! `doc/raycast-backend-protocol.md`: chat messages nest their body under
//! `content.text` (never a bare string), the request uses `system_instructions`
//! (plural), and the response is *always* an SSE stream whose payloads carry
//! `reasoning` / `text` deltas, whole `tool_calls`, `usage`, and a
//! `thought_signature` tucked inside `extra_content.<provider>`.
//!
//! Only the [builder](super::builder) writes these request types and only the
//! [parser](super::parser) reads the response types, keeping every other layer
//! ignorant of Raycast's wire format.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// --- Request ------------------------------------------------------------

/// A single message in a Raycast chat request.
///
/// `content` is always the object `{ "text": ... }`. `tool_calls`, `name`,
/// `tool_call_id`, and `extra_content` only appear on the message kinds that
/// need them (assistant tool calls, tool results, think signatures) and are
/// omitted otherwise so a plain text request serializes exactly as before.
#[derive(Debug, Clone, Serialize)]
pub struct RaycastMessage {
    pub role: String,
    pub content: RaycastMessageContent,
    /// Assistant tool calls (OpenAI-nested form). Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<RaycastRequestToolCall>,
    /// Tool name, present only on `role: "tool"` result messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Id of the call being answered, present only on tool result messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Carries `{ "<provider>": { "thought_signature": ... } }` on assistant
    /// turns that replay a think signature. Omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_content: Option<Value>,
}

impl RaycastMessage {
    /// A plain `{role, content:{text}}` message with no tool/think metadata.
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: RaycastMessageContent { text: text.into() },
            tool_calls: Vec::new(),
            name: None,
            tool_call_id: None,
            extra_content: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaycastMessageContent {
    pub text: String,
}

/// Assistant tool call as written back into a *request* `messages[].tool_calls`
/// — the OpenAI-nested form where `arguments` is a JSON **string**.
#[derive(Debug, Clone, Serialize)]
pub struct RaycastRequestToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: RaycastRequestToolFunction,
}

#[derive(Debug, Clone, Serialize)]
pub struct RaycastRequestToolFunction {
    pub name: String,
    pub arguments: String,
}

impl RaycastRequestToolCall {
    /// Build a `type: "function"` tool call from a canonical call.
    pub fn function(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: "function",
            function: RaycastRequestToolFunction {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }
}

/// Body POSTed to the `chat_completions` endpoint.
///
/// `thread_id`, `message_id`, `buffer_id`, and `current_date` are always
/// populated per the backend contract: `thread_id` is derived per conversation
/// (session affinity), `message_id` is derived per user turn (stable across a
/// turn's tool round-trips), and `buffer_id` is minted fresh per request — see
/// the builder for the derivations and the capture they were verified against.
/// The remaining fields are `skip_serializing_if`-guarded, so a plain OpenAI
/// chat request stays minimal and they light up only for richer protocols/think.
#[derive(Debug, Clone, Serialize)]
pub struct RaycastChatRequest {
    pub model: String,
    pub provider: String,
    pub messages: Vec<RaycastMessage>,
    pub system_instructions: String,
    pub temperature: f64,
    pub source: String,
    pub locale: String,
    pub thread_id: String,
    /// Tool definitions; empty for plain chat. `Value` keeps the door open for
    /// forwarding local/remote tool schemas without a type change.
    pub tools: Vec<Value>,
    /// Appended system prompt (memory/skills). Omitted when unused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_system_instructions: Option<String>,
    /// `low` | `medium` | `high` — present only for think models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Tool-choice policy (`auto`, `none`, ...). Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
    /// UUID identifying the current user turn; stable across that turn's tool
    /// round-trips, rotates on each new user message (derived in the builder).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// UUID scratch buffer id; minted fresh per request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer_id: Option<String>,
    /// `YYYY-MM-DD` the client believes it is. Omitted when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_date: Option<String>,
}

// --- Response (SSE) -----------------------------------------------------

/// One SSE `data:` payload from the chat stream.
///
/// Every field is optional because a single stream mixes notifications, ping
/// keep-alives, reasoning deltas, text deltas, and a terminating usage/finish
/// event. `notification_type` lets the parser cheaply recognize and drop the
/// non-content events. Unknown fields (costs, `status_page_url`, ...) are
/// ignored.
#[derive(Debug, Default, Deserialize)]
pub struct RaycastSseData {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<RaycastSseToolCall>,
    #[serde(default)]
    pub usage: Option<RaycastUsage>,
    #[serde(default)]
    pub extra_content: Option<Value>,
    /// Set on `provider_status` / `ping` notification events (content events
    /// omit it), so the parser can skip them without misreading a delta.
    #[serde(default)]
    pub notification_type: Option<String>,
}

/// Tool call as delivered in the *stream* — the flat `{name, arguments, id}`
/// form (note: different from the nested request form above).
#[derive(Debug, Clone, Deserialize)]
pub struct RaycastSseToolCall {
    pub id: String,
    pub name: String,
    /// A JSON **string** of the arguments, kept verbatim.
    pub arguments: String,
}

/// Token accounting from the terminating event. Cost fields are ignored.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct RaycastUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub cached_tokens: u32,
}

impl RaycastSseData {
    /// Extract the provider-agnostic `thought_signature`, wherever it is nested
    /// (`extra_content.anthropic.thought_signature` or `...openai...`).
    pub fn thought_signature(&self) -> Option<String> {
        let extra = self.extra_content.as_ref()?;
        let obj = extra.as_object()?;
        obj.values()
            .find_map(|v| v.get("thought_signature"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }
}

// --- Models -------------------------------------------------------------

/// A raw model entry from `GET /models`.
#[derive(Debug, Clone, Deserialize)]
pub struct RaycastRawModel {
    pub id: String,
    pub model: String,
    pub provider: String,
    #[serde(default)]
    pub requires_better_ai: bool,
    #[serde(default)]
    pub availability: Option<String>,
}

/// Top-level shape of the `GET /models` response.
#[derive(Debug, Deserialize)]
pub struct RaycastModelsResponse {
    pub models: Vec<RaycastRawModel>,
}

// --- 账号信息查询(/me + /ai/credits) --------------------------------

/// `GET /api/v1/me` 的瘦响应:只取订阅相关字段。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RaycastMe {
    #[serde(default)]
    pub subscription: Option<RaycastSubscription>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RaycastSubscription {
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub trial_days_left: Option<i64>,
    #[serde(default)]
    pub current_period_end: Option<i64>,
}

/// `GET /api/v1/ai/credits` 的瘦响应。余额字段可能以数字或字符串下发。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RaycastCredits {
    #[serde(default, deserialize_with = "string_or_number_f64")]
    pub remaining_balance_credits: Option<f64>,
    #[serde(default, deserialize_with = "string_or_number_f64")]
    pub total_balance_credits: Option<f64>,
    #[serde(default, deserialize_with = "string_or_number_f64")]
    pub remaining_balance_usd: Option<f64>,
    #[serde(default)]
    pub next_credits_at: Option<String>,
}

/// 接受 JSON number 或可解析的字符串,统一为 `Option<f64>`。
fn string_or_number_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }))
}

#[cfg(test)]
mod credits_tests {
    use super::*;

    #[test]
    fn credits_parse_number_and_string_forms() {
        let c: RaycastCredits = serde_json::from_str(
            r#"{ "remaining_balance_credits": "5.5", "total_balance_credits": 10,
                "remaining_balance_usd": "0.55", "next_credits_at": "2026-10-01T00:00:00Z" }"#,
        )
        .unwrap();
        assert_eq!(c.remaining_balance_credits, Some(5.5));
        assert_eq!(c.total_balance_credits, Some(10.0));
        assert_eq!(c.remaining_balance_usd, Some(0.55));
        assert_eq!(c.next_credits_at.as_deref(), Some("2026-10-01T00:00:00Z"));
    }
}
