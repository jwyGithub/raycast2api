//! Compaction recipe: trigger estimate, summarization, history rewrite.

use crate::core::message::{Message, Role};
use crate::core::request::CanonicalRequest;

use super::cache::{CompactCache, message_bytes};

/// Fraction of the model context window at which compaction triggers.
/// Codex: `auto_compact_token_limit = context_window * 9 / 10`.
const AUTO_COMPACT_RATIO: f64 = 0.9;

/// Skip compaction entirely below this window size — small models on the list
/// would otherwise compact on nearly every turn.
const MIN_COMPACT_WINDOW_TOKENS: u64 = 32_000;

/// Token budget for retained recent user messages (Codex:
/// `COMPACT_USER_MESSAGE_MAX_TOKENS`).
const RETAINED_USER_MESSAGE_BUDGET: usize = 20_000;

/// Safety margin subtracted from the context threshold to leave room for
/// estimator error and the completion itself.
const TRIGGER_MARGIN_TOKENS: u64 = 8_000;

/// (`codex-rs/prompts/templates/compact/prompt.md`).
pub const SUMMARIZATION_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.\n\nInclude:\n- Current progress and key decisions made\n- Important context, constraints, or user preferences\n- What remains to be done (clear next steps)\n- Any critical data, examples, or references needed to continue\n\nBe concise, structured, and focused on helping the next LLM seamlessly continue the work.";

/// The fixed prefix marking a compaction summary (Codex:
/// `SUMMARY_PREFIX`), so `is_summary` can recognize prior summaries.
pub const SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// Rough token estimate: ~4 UTF-8 bytes per token.
pub(super) fn approx_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Whether a user message is a previously produced compaction summary.
pub(super) fn is_summary(message: &Message) -> bool {
    message
        .text
        .starts_with(format!("{SUMMARY_PREFIX}\n").as_str())
}

/// The outcome of a compaction attempt.
pub(crate) enum Compaction {
    /// Nothing needed (or possible) — run the request as-is.
    NotNeeded,
    /// History to summarize, excluding the current user turn.
    Summarize(Vec<Message>),
    /// The complete historical prefix already has a cached summary.
    Cached(String),
}

pub(crate) async fn apply(
    cache: &CompactCache,
    mut request: CanonicalRequest,
    plan: Compaction,
    send: impl FnOnce(
        CanonicalRequest,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<String, crate::error::GatewayError>,
    >,
) -> Result<CanonicalRequest, crate::error::GatewayError> {
    match plan {
        Compaction::NotNeeded => Ok(request),
        Compaction::Cached(summary) => {
            request.messages = rebuild(&request.messages, &summary);
            Ok(request)
        }
        Compaction::Summarize(messages) => {
            let summary_text = summarize_messages(&request, messages, send).await?;
            let messages = rebuild(&request.messages, &summary_text);
            tracing::info!(
                model = %request.model,
                summary_chars = summary_text.len(),
                original_messages = request.messages.len(),
                compacted_messages = messages.len(),
                "compact: summary completed — history rewritten"
            );

            cache.record_summary(&request, summary_text).await;
            request.messages = messages;
            Ok(request)
        }
    }
}

/// Run the summarization completion over `messages` and return the
/// [`SUMMARY_PREFIX`]-prefixed summary. The summarization prompt replaces the
/// client's system prompt and tools; the call is a plain completion.
pub(crate) async fn summarize_messages(
    request: &CanonicalRequest,
    messages: Vec<Message>,
    send: impl FnOnce(
        CanonicalRequest,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<String, crate::error::GatewayError>,
    >,
) -> Result<String, crate::error::GatewayError> {
    let summary_request = CanonicalRequest {
        requested_effort: request.requested_effort.clone(),
        requested_thinking: request.requested_thinking.clone(),
        stream: false,
        temperature: None,
        max_tokens: None,
        tools: Vec::new(),
        tool_choice: None,
        messages: messages
            .into_iter()
            .chain(std::iter::once(Message::text(
                Role::User,
                SUMMARIZATION_PROMPT,
            )))
            .collect(),
        ..CanonicalRequest::new(request.protocol, request.model.clone())
    };
    let summary = send(summary_request).await.map_err(|error| {
        tracing::error!(
            model = %request.model,
            messages = request.messages.len(),
            %error,
            "compact: summarization call failed — request aborted"
        );
        error
    })?;
    if summary.trim().is_empty() {
        return Err(crate::error::GatewayError::Upstream {
            status: 502,
            message: "Compaction returned an empty summary".into(),
        });
    }
    Ok(format!("{SUMMARY_PREFIX}\n{summary}"))
}

/// Decide whether the request needs compaction, and compute the rewritten
/// history when it does (without the summary yet).
pub(crate) async fn plan(
    cache: &CompactCache,
    request: &CanonicalRequest,
    model: &crate::raycast::wire::RaycastRawModel,
) -> Compaction {
    // The authorized catalog entry carries the context window in K tokens.
    let Some(context_k) = model
        .extra
        .get("context")
        .and_then(serde_json::Value::as_u64)
    else {
        return Compaction::NotNeeded;
    };
    let window = context_k * 1_000;
    if window < MIN_COMPACT_WINDOW_TOKENS {
        return Compaction::NotNeeded;
    }

    let measured_tokens = cache.estimate_input_tokens(request).await;
    let request_tokens = measured_tokens.unwrap_or_else(|| estimate_request_tokens(request) as u64);
    let threshold = (window as f64 * AUTO_COMPACT_RATIO) as u64 - TRIGGER_MARGIN_TOKENS;
    if request_tokens < threshold {
        return Compaction::NotNeeded;
    }
    tracing::info!(
        model = %request.model,
        estimated_tokens = request_tokens,
        token_source = if measured_tokens.is_some() { "usage+estimate" } else { "estimate" },
        threshold,
        window,
        "compact: threshold reached, evaluating turn boundary"
    );

    // Only compact at a turn boundary: the trailing message must be the new
    // user turn (Codex compacts pre-turn, never mid-turn).
    let Some(last) = request.messages.last() else {
        return Compaction::NotNeeded;
    };
    if last.role != Role::User || is_summary(last) {
        tracing::debug!(
            last_role = ?last.role,
            "compact: skipped — not at a user turn boundary"
        );
        return Compaction::NotNeeded;
    }

    // Reuse a cached summary covering the largest prefix, then summarize only
    // the delta. The trailing user message is never summarized — it is the
    // turn we must answer.
    let droppable = request.messages.len() - 1;
    let cache_hit = cache.cached_summary(request).await;
    let (covered, previous_summary) = cache_hit.unwrap_or((0, String::new()));
    tracing::info!(
        model = %request.model,
        messages = request.messages.len(),
        cache_hit_messages = covered,
        incremental = !previous_summary.is_empty(),
        "compact: TRIGGERED — summarizing conversation prefix"
    );

    if covered == droppable && !previous_summary.is_empty() {
        return Compaction::Cached(previous_summary);
    }
    // Cache offsets refer to the original history, before removing old summaries.
    let mut messages: Vec<Message> = request.messages[covered..droppable]
        .iter()
        .filter(|m| !(m.role == Role::User && is_summary(m)))
        .cloned()
        .collect();
    if !previous_summary.is_empty() {
        messages.insert(0, Message::text(Role::User, previous_summary));
    }
    Compaction::Summarize(messages)
}

/// Rebuild the conversation the way Codex's `build_compacted_history` does:
/// recent user messages (newest-first budget of
/// [`RETAINED_USER_MESSAGE_BUDGET`] tokens, skipping old summaries) followed by
/// the fresh summary, then the trailing user turn.
pub(super) fn rebuild(messages: &[Message], summary_text: &str) -> Vec<Message> {
    let trailing = messages.last().cloned();
    let kept_budget = RETAINED_USER_MESSAGE_BUDGET as u64;
    let mut retained: Vec<Message> = Vec::new();
    let mut remaining = kept_budget;
    for message in messages[..messages.len().saturating_sub(1)].iter().rev() {
        if remaining == 0 {
            break;
        }
        if message.role != Role::User || is_summary(message) || message.text.is_empty() {
            continue;
        }
        let tokens = approx_tokens(&message.text);
        if tokens > remaining {
            break;
        }
        remaining -= tokens;
        retained.push(message.clone());
    }
    retained.reverse();

    let mut out = Vec::with_capacity(retained.len() + 2);
    out.extend(retained);
    out.push(Message::text(Role::User, summary_text));
    if let Some(trailing) = trailing {
        out.push(trailing);
    }
    out
}

/// Estimate the token footprint of a canonical request: every message text,
/// tool-call arguments, the system prompt and the tool schemas.
pub(super) fn estimate_request_tokens(request: &CanonicalRequest) -> usize {
    let mut bytes = message_bytes(&request.messages);
    if let Some(system) = &request.system {
        bytes += system.len();
    }
    for tool in &request.tools {
        // Schema shape via Debug formatting — ToolDef doesn't impl Serialize,
        // and an approximate footprint is all the estimator needs.
        bytes += format!("{tool:?}").len();
    }
    bytes / 4
}
