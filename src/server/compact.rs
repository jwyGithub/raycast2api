//! Server-side **auto compact** for long conversations.
//!
//! Ported from the Codex harness's local compaction path
//! (`codex-rs/core/src/compact.rs` + `prompts/templates/compact`). Codex
//! detects that the accumulated conversation approaches the model's context
//! window, sends the history plus a summarization prompt through the model
//! once, then rebuilds the history as *recent user messages + summary*. The
//! summary is recognizable by a fixed prefix so later compactions skip it.
//!
//! The gateway is stateless — clients like Codex resend the full history every
//! turn — so this module reimplements the same recipe at the protocol boundary:
//!
//! 1. **Estimate** request tokens using upstream input usage for a matching
//!    history prefix plus bytes/4 for new messages; fall back to bytes/4 for
//!    the full request when no prefix matches. Compare against the model's
//!    `context` (in K tokens from Raycast `GET /models`), triggering at
//!    [`AUTO_COMPACT_RATIO`] of the window minus [`TRIGGER_MARGIN_TOKENS`].
//! 2. **Summarize**: the dropped prefix plus [`SUMMARIZATION_PROMPT`] (the
//!    verbatim Codex template) goes through the upstream as one extra
//!    completion; the final assistant text is the summary.
//! 3. **Rewrite**: canonical history becomes *recent user messages (within the
//!    [`RETAINED_USER_MESSAGE_BUDGET`] token budget, oldest-dropped-first like
//!    Codex) + the prefixed summary + the trailing turn*.
//!
//! Summaries are cached by the hash of the dropped prefix, so on the next turn
//! — whose history is a superset — only the not-yet-summarized suffix needs a
//! second pass. Cache entries expire after [`SUMMARY_CACHE_TTL`].

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::core::message::{Message, Role};
use crate::core::request::CanonicalRequest;

use super::state::SharedState;

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

/// How long a cached summary stays valid.
const SUMMARY_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const USAGE_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const USAGE_CACHE_CAPACITY: usize = 1_024;

/// The verbatim Codex summarization template
/// (`codex-rs/prompts/templates/compact/prompt.md`).
pub const SUMMARIZATION_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.\n\nInclude:\n- Current progress and key decisions made\n- Important context, constraints, or user preferences\n- What remains to be done (clear next steps)\n- Any critical data, examples, or references needed to continue\n\nBe concise, structured, and focused on helping the next LLM seamlessly continue the work.";

/// The fixed prefix marking a compaction summary (Codex:
/// `SUMMARY_PREFIX`), so `is_summary` can recognize prior summaries.
pub const SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// Rough token estimate: ~4 UTF-8 bytes per token.
fn approx_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Identifies the exact input whose token count arrives at stream completion.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct UsageKey {
    prefix_hash: [u8; 32],
    covered_messages: usize,
}

impl UsageKey {
    pub(crate) fn new(request: &CanonicalRequest) -> Self {
        Self {
            prefix_hash: *usage_prefix_hashes(request).last().unwrap(),
            covered_messages: request.messages.len(),
        }
    }
}

struct CachedUsage {
    key: UsageKey,
    input_tokens: u32,
    created_at: Instant,
}

/// Bounded input-token measurements; stores hashes rather than conversation text.
#[derive(Default)]
pub struct UsageCache {
    entries: Mutex<VecDeque<CachedUsage>>,
}

impl UsageCache {
    pub(crate) async fn record(&self, key: UsageKey, input_tokens: u32) {
        // Missing upstream usage is parsed as zero and cannot calibrate a prefix.
        if input_tokens == 0 || key.covered_messages == 0 {
            return;
        }
        let mut entries = self.entries.lock().await;
        let now = Instant::now();
        entries.retain(|entry| {
            now.duration_since(entry.created_at) < USAGE_CACHE_TTL && entry.key != key
        });
        if entries.len() >= USAGE_CACHE_CAPACITY {
            entries.pop_front();
        }
        entries.push_back(CachedUsage {
            key,
            input_tokens,
            created_at: now,
        });
    }

    pub(crate) async fn estimate_tokens(&self, request: &CanonicalRequest) -> Option<u64> {
        let hashes = usage_prefix_hashes(request);
        let entries = self.entries.lock().await;
        let now = Instant::now();
        let entry = entries
            .iter()
            .filter(|entry| {
                now.duration_since(entry.created_at) < USAGE_CACHE_TTL
                    && hashes.get(entry.key.covered_messages) == Some(&entry.key.prefix_hash)
            })
            .max_by_key(|entry| entry.key.covered_messages)?;
        let added_bytes = message_bytes(&request.messages[entry.key.covered_messages..]);
        Some(u64::from(entry.input_tokens) + (added_bytes as u64).div_ceil(4))
    }
}

/// Every prefix includes the prompt configuration, so unrelated inputs never
/// share measurements merely because they use the same model or first message.
fn usage_prefix_hashes(request: &CanonicalRequest) -> Vec<[u8; 32]> {
    let mut hash = Sha256::new();
    hash.update(
        format!(
            "{:?}",
            (
                &request.model,
                &request.system,
                &request.tools,
                &request.tool_choice,
                &request.reasoning_effort,
            )
        )
        .as_bytes(),
    );
    let mut hashes = Vec::with_capacity(request.messages.len() + 1);
    hashes.push(hash.clone().finalize().into());
    for message in &request.messages {
        // The builder never replays thinking text; clients may omit it next turn.
        hash.update(
            format!(
                "{:?}",
                (
                    message.role.as_str(),
                    &message.text,
                    &message.tool_calls,
                    &message.tool_call_id,
                    &message.tool_name,
                    message.tool_error,
                )
            )
            .as_bytes(),
        );
        hashes.push(hash.clone().finalize().into());
    }
    hashes
}

/// Whether a user message is a previously produced compaction summary.
fn is_summary(message: &Message) -> bool {
    message
        .text
        .starts_with(format!("{SUMMARY_PREFIX}\n").as_str())
}

/// A cached summary keyed by the hash of the dropped prefix it covers.
struct CachedSummary {
    /// FNV-1a hash of the summarized prefix (message index + text seeds).
    prefix_hash: u64,
    /// Number of leading messages the summary covers.
    covered_messages: usize,
    summary_text: String,
    created_at: Instant,
}

/// Per-model-id summary cache shared across requests.
#[derive(Default)]
pub struct SummaryCache {
    entries: Mutex<HashMap<String, Vec<CachedSummary>>>,
}

impl SummaryCache {
    /// Record that `summary_text` summarizes the first `covered_messages`
    /// messages of the conversation hashed as `prefix_hash` under `model`.
    async fn record(
        &self,
        model: &str,
        prefix_hash: u64,
        covered_messages: usize,
        summary_text: String,
    ) {
        let mut entries = self.entries.lock().await;
        let now = Instant::now();
        entries.retain(|_, list| {
            list.retain(|c| now.duration_since(c.created_at) < SUMMARY_CACHE_TTL);
            !list.is_empty()
        });
        if !entries.contains_key(model)
            && entries.len() >= 32
            && let Some(key) = entries.keys().next().cloned()
        {
            entries.remove(&key);
        }
        let list = entries.entry(model.to_string()).or_default();
        let now = Instant::now();
        list.retain(|c| now.duration_since(c.created_at) < SUMMARY_CACHE_TTL);
        if list.len() >= 128 {
            list.remove(0);
        }
        list.push(CachedSummary {
            prefix_hash,
            covered_messages,
            summary_text,
            created_at: now,
        });
    }

    /// Find the longest cached summary covering a prefix of this conversation.
    /// `history_hash_at(n)` must return the same value recorded at
    /// [`SummaryCache::record`] time for the first `n` messages.
    async fn lookup(
        &self,
        model: &str,
        history_hash_at: impl Fn(usize) -> u64,
        total: usize,
    ) -> Option<(usize, String)> {
        let entries = self.entries.lock().await;
        let list = entries.get(model)?;
        let now = Instant::now();
        list.iter()
            .filter(|c| {
                now.duration_since(c.created_at) < SUMMARY_CACHE_TTL
                    && c.covered_messages <= total
                    && c.prefix_hash == history_hash_at(c.covered_messages)
            })
            // Longest cover wins: the least leftover work for the delta pass.
            .max_by_key(|c| c.covered_messages)
            .map(|c| (c.covered_messages, c.summary_text.clone()))
    }
}

/// FNV-1a 64-bit over message texts — stable across turns for an unchanged
/// prefix (no external hashing crate needed).
fn hash_messages(messages: &[Message], upto: usize) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    fn feed(hash: &mut u64, bytes: &[u8]) {
        for b in bytes {
            *hash ^= u64::from(*b);
            *hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    for message in &messages[..upto.min(messages.len())] {
        feed(&mut hash, message.role.as_str().as_bytes());
        feed(&mut hash, message.text.as_bytes());
        for call in &message.tool_calls {
            feed(&mut hash, call.id.as_bytes());
            feed(&mut hash, call.name.as_bytes());
            feed(&mut hash, call.arguments.as_bytes());
        }
        feed(
            &mut hash,
            message.thinking.as_ref().map_or("", |t| &t.text).as_bytes(),
        );
    }
    hash
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

/// Inspect an incoming request and, when the conversation has outgrown the
/// model's context window, rewrite it into the compacted form. `send` runs a
/// model completion (used for the summarization call itself).
pub async fn maybe_compact(
    state: &SharedState,
    request: CanonicalRequest,
    send: impl FnOnce(
        CanonicalRequest,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<String, crate::error::GatewayError>,
    >,
) -> Result<CanonicalRequest, crate::error::GatewayError> {
    let plan = plan(state, &request).await;
    apply(state, request, plan, send).await
}

pub(crate) async fn apply(
    state: &SharedState,
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

            state
                .summary_cache
                .record(
                    &request.model,
                    request_compact_hash(&request),
                    request.messages.len().saturating_sub(1),
                    summary_text,
                )
                .await;
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
pub(crate) async fn plan(state: &SharedState, request: &CanonicalRequest) -> Compaction {
    let models = match state.models().await {
        Ok(models) => models,
        Err(_) => return Compaction::NotNeeded,
    };
    let Some(entry) = models.iter().find(|m| m.id == request.model) else {
        return Compaction::NotNeeded;
    };
    // `context` is in K tokens (Raycast `GET /models`); lives in the passthrough `extra`.
    let Some(context_k) = entry
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

    let measured_tokens = state.usage_cache.estimate_tokens(request).await;
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
    let cache_hit = state
        .summary_cache
        .lookup(
            &request.model,
            |n| hash_messages(&request.messages, n),
            droppable,
        )
        .await;
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
fn rebuild(messages: &[Message], summary_text: &str) -> Vec<Message> {
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
fn estimate_request_tokens(request: &CanonicalRequest) -> usize {
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

fn message_bytes(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| {
            message.text.len()
                + message
                    .tool_calls
                    .iter()
                    .map(|call| call.arguments.len() + call.name.len())
                    .sum::<usize>()
        })
        .sum()
}

/// Hash identifying the droppable prefix of a request (cache key basis).
fn request_compact_hash(request: &CanonicalRequest) -> u64 {
    hash_messages(&request.messages, request.messages.len().saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::protocol::Protocol;
    use crate::core::tools::ToolDef;

    fn request(messages: Vec<Message>) -> CanonicalRequest {
        let mut request = CanonicalRequest::new(Protocol::Responses, "anthropic-claude-haiku-5-5");
        request.messages = messages;
        request
    }

    fn filler_messages(count: usize, chars_each: usize) -> Vec<Message> {
        (0..count)
            .map(|i| {
                if i % 2 == 0 {
                    Message::text(Role::User, "u".repeat(chars_each))
                } else {
                    Message::text(Role::Assistant, "a".repeat(chars_each))
                }
            })
            .collect()
    }

    #[test]
    fn approximates_tokens() {
        assert_eq!(approx_tokens(""), 0);
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("abcde"), 2);
    }

    #[test]
    fn recognizes_summaries_by_prefix() {
        let plain = Message::text(Role::User, "hello");
        assert!(!is_summary(&plain));
        let summary = Message::text(Role::User, format!("{SUMMARY_PREFIX}\nthe summary body"));
        assert!(is_summary(&summary));
    }

    #[test]
    fn rebuild_keeps_recent_users_budget_then_summary_then_trailing() {
        let mut messages = filler_messages(4, 4000); // each ~1000 tokens
        messages.push(Message::text(Role::User, "answer this"));
        let rebuilt = rebuild(&messages, "SUMMARY");
        // budget 20k keeps all four user/assistant? No — only user messages.
        // filler has users at 0, 2 → both kept (~2000 tokens), summary, trailing.
        assert_eq!(rebuilt.len(), 4);
        assert_eq!(rebuilt[0].role, Role::User);
        assert_eq!(rebuilt[1].role, Role::User);
        assert_eq!(rebuilt[2].text, "SUMMARY");
        assert_eq!(rebuilt[3].text, "answer this");
    }

    #[test]
    fn rebuild_drops_old_users_past_budget() {
        let mut messages = filler_messages(60, 4000); // 30 users × 1000 tokens
        messages.push(Message::text(Role::User, "final"));
        let rebuilt = rebuild(&messages, "S");
        // 20k budget → 20 users max; + summary + trailing = 22
        assert_eq!(rebuilt.len(), 22);
        assert_eq!(rebuilt[21].text, "final");
        assert_eq!(rebuilt[20].text, "S");
    }

    #[test]
    fn hash_is_prefix_stable() {
        let mut messages = filler_messages(10, 100);
        let h5 = hash_messages(&messages, 5);
        messages.push(Message::text(Role::User, "more"));
        // Appending later messages must not change the prefix hash.
        assert_eq!(hash_messages(&messages, 5), h5);
        assert_ne!(hash_messages(&messages, 6), h5);
    }

    #[test]
    fn estimator_counts_system_messages_and_tool_args() {
        let mut request = request(vec![Message::text(Role::User, "12345678")]); // 8 chars → 2
        request.system = Some("12345678".into()); // +2
        request.tools.push(ToolDef::Local {
            name: "t".into(),
            description: None,
            parameters: serde_json::json!({}),
        });
        let tokens = estimate_request_tokens(&request);
        assert!(tokens > 4); // at least the two 2-token texts plus tool debug
    }

    #[tokio::test]
    async fn usage_counts_exact_prefix_plus_only_new_message_bytes() {
        let cache = UsageCache::default();
        let mut original = request(vec![Message::text(Role::User, "seed")]);
        original.system = Some("system".repeat(100));
        original.tools.push(ToolDef::Remote { name: "t".into() });
        cache.record(UsageKey::new(&original), 46_690).await;
        assert_eq!(cache.estimate_tokens(&original).await, Some(46_690));

        let mut reply = Message::text(Role::Assistant, "reply");
        reply.thinking = Some(crate::core::message::Thinking {
            text: "thinking".repeat(1000),
            signature: None,
        });
        reply.tool_calls.push(crate::core::message::ToolCall {
            id: "call_1".into(),
            name: "get".into(),
            arguments: "{}".into(),
        });
        original.messages.extend([
            reply,
            Message::text(Role::Tool, "结果"),
            Message::text(Role::User, "next"),
        ]);
        // 5 reply + 3 name + 2 arguments + 6 UTF-8 tool result + 4 user bytes.
        assert_eq!(cache.estimate_tokens(&original).await, Some(46_695));
        original.messages.last_mut().unwrap().text.push('!');
        assert_eq!(cache.estimate_tokens(&original).await, Some(46_696));
    }

    #[tokio::test]
    async fn usage_rejects_changed_prompt_configuration_or_history() {
        let cache = UsageCache::default();
        let mut original = request(vec![
            Message::text(Role::User, "seed"),
            Message::text(Role::Assistant, "calling"),
            Message::text(Role::Tool, "result"),
        ]);
        original.messages[1]
            .tool_calls
            .push(crate::core::message::ToolCall {
                id: "call_1".into(),
                name: "get".into(),
                arguments: "{}".into(),
            });
        cache.record(UsageKey::new(&original), 1000).await;
        for field in [
            "model",
            "system",
            "tools",
            "tool_choice",
            "effort",
            "role",
            "text",
            "call_id",
            "call_name",
            "arguments",
            "result_id",
            "result_name",
            "result_error",
            "truncated",
        ] {
            let mut changed = original.clone();
            match field {
                "model" => changed.model.push('x'),
                "system" => changed.system = Some("different system".into()),
                "tools" => changed.tools.push(ToolDef::Remote { name: "t".into() }),
                "tool_choice" => changed.tool_choice = Some(crate::core::tools::ToolChoice::None),
                "effort" => {
                    changed.reasoning_effort = Some(crate::core::request::ReasoningEffort::High)
                }
                "role" => changed.messages[0].role = Role::Assistant,
                "text" => changed.messages[0].text.push('x'),
                "call_id" => changed.messages[1].tool_calls[0].id.push('x'),
                "call_name" => changed.messages[1].tool_calls[0].name.push('x'),
                "arguments" => changed.messages[1].tool_calls[0].arguments.push('x'),
                "result_id" => changed.messages[2].tool_call_id = Some("other".into()),
                "result_name" => changed.messages[2].tool_name = Some("other".into()),
                "result_error" => changed.messages[2].tool_error = true,
                "truncated" => {
                    changed.messages.pop();
                }
                _ => unreachable!(),
            }
            assert_eq!(cache.estimate_tokens(&changed).await, None, "{field}");
        }
        // Changes the builder does not replay must not invalidate input usage.
        original.messages[1].thinking = Some(crate::core::message::Thinking {
            text: "private thinking".into(),
            signature: None,
        });
        original.stream = true;
        assert_eq!(cache.estimate_tokens(&original).await, Some(1000));
    }

    #[tokio::test]
    async fn usage_selects_longest_matching_prefix_among_concurrent_branches() {
        let cache = UsageCache::default();
        let seed = request(vec![Message::text(Role::User, "seed")]);
        let mut branch_a = seed.clone();
        branch_a.messages.extend([
            Message::text(Role::Assistant, "aaaa"),
            Message::text(Role::User, "aaaa"),
        ]);
        let mut branch_b = branch_a.clone();
        branch_b.messages[2].text = "bbbb".into();
        tokio::join!(
            cache.record(UsageKey::new(&branch_a), 2000),
            cache.record(UsageKey::new(&branch_b), 3000),
        );
        // Even a newer short prefix must not replace the longer match.
        cache.record(UsageKey::new(&seed), 1000).await;
        branch_a
            .messages
            .push(Message::text(Role::Assistant, "next"));
        assert_eq!(cache.estimate_tokens(&branch_a).await, Some(2001));
        assert_eq!(cache.estimate_tokens(&branch_b).await, Some(3000));
        cache.record(UsageKey::new(&branch_b), 3500).await;
        assert_eq!(cache.estimate_tokens(&branch_b).await, Some(3500));
    }

    #[tokio::test]
    async fn usage_cache_ignores_zero_expires_and_stays_bounded() {
        let cache = UsageCache::default();
        let original = request(vec![Message::text(Role::User, "seed")]);
        let key = UsageKey::new(&original);
        cache.record(key, 1000).await;
        cache.record(key, 0).await;
        assert_eq!(cache.estimate_tokens(&original).await, Some(1000));
        cache.entries.lock().await[0].created_at = Instant::now() - USAGE_CACHE_TTL;
        assert_eq!(cache.estimate_tokens(&original).await, None);

        for i in 0..=USAGE_CACHE_CAPACITY {
            let other = request(vec![Message::text(Role::User, i.to_string())]);
            cache.record(UsageKey::new(&other), 1000).await;
        }
        assert_eq!(cache.entries.lock().await.len(), USAGE_CACHE_CAPACITY);
        let evicted = request(vec![Message::text(Role::User, "0")]);
        assert_eq!(cache.estimate_tokens(&evicted).await, None);
    }

    #[tokio::test]
    async fn plan_uses_measured_usage_instead_of_full_request_estimate() {
        let state = super::super::state::compact_test_state();
        for (bytes, measured, should_compact) in [(90_000, 100, false), (8, 21_000, true)] {
            let mut original = CanonicalRequest::new(Protocol::Responses, "test-model");
            original.messages = vec![Message::text(Role::User, "x".repeat(bytes))];
            state
                .usage_cache
                .record(UsageKey::new(&original), measured)
                .await;
            original.messages.extend([
                Message::text(Role::Assistant, "done"),
                Message::text(Role::User, "next"),
            ]);
            assert_eq!(
                matches!(plan(&state, &original).await, Compaction::Summarize(_)),
                should_compact,
            );
        }
    }

    #[tokio::test]
    async fn compacted_usage_does_not_calibrate_original_client_history() {
        let state = super::super::state::compact_test_state();
        let mut original = CanonicalRequest::new(Protocol::Responses, "test-model");
        original.messages = vec![
            Message::text(Role::User, "old"),
            Message::text(Role::Assistant, "x".repeat(90_000)),
            Message::text(Role::User, "CURRENT_TASK"),
        ];
        let compacted = maybe_compact(&state, original.clone(), |_| {
            Box::pin(async { Ok("summary".into()) })
        })
        .await
        .unwrap();
        state
            .usage_cache
            .record(UsageKey::new(&compacted), 100)
            .await;
        assert_eq!(state.usage_cache.estimate_tokens(&original).await, None);
        original.messages.extend([
            Message::text(Role::Assistant, "done"),
            Message::text(Role::User, "NEXT_TASK"),
        ]);
        assert!(matches!(
            plan(&state, &original).await,
            Compaction::Summarize(_)
        ));
        let mut continued = compacted;
        continued.messages.extend([
            Message::text(Role::Assistant, "done"),
            Message::text(Role::User, "next"),
        ]);
        assert_eq!(
            state.usage_cache.estimate_tokens(&continued).await,
            Some(102)
        );
    }

    #[tokio::test]
    async fn compaction_preserves_current_task_and_reuses_exact_cache() {
        let state = super::super::state::compact_test_state();
        let mut original = CanonicalRequest::new(Protocol::Responses, "test-model");
        original.messages = vec![
            Message::text(Role::User, "old"),
            Message::text(Role::Assistant, "x".repeat(90_000)),
            Message::text(Role::User, "CURRENT_TASK"),
        ];
        original.system = Some("system".into());
        original.stream = true;
        original.max_tokens = Some(123);
        original.temperature = Some(0.2);
        original.reasoning_effort = Some(crate::core::request::ReasoningEffort::High);
        original.tools.push(ToolDef::Remote {
            name: "tool".into(),
        });
        original.tool_choice = Some(crate::core::tools::ToolChoice::Auto);
        let rewritten = maybe_compact(&state, original.clone(), |summary| {
            Box::pin(async move {
                assert_eq!(summary.messages.len(), 3);
                assert_eq!(summary.messages.last().unwrap().text, SUMMARIZATION_PROMPT);
                assert!(!summary.messages.iter().any(|m| m.text == "CURRENT_TASK"));
                assert!(summary.system.is_none());
                assert!(summary.tools.is_empty());
                Ok("summary".into())
            })
        })
        .await
        .unwrap();
        assert_eq!(rewritten.messages.last().unwrap().text, "CURRENT_TASK");
        assert_eq!(rewritten.messages.last().unwrap().role, Role::User);
        assert!(is_summary(
            &rewritten.messages[rewritten.messages.len() - 2]
        ));
        assert_eq!(rewritten.system, original.system);
        assert_eq!(rewritten.stream, original.stream);
        assert_eq!(rewritten.max_tokens, original.max_tokens);
        assert_eq!(rewritten.temperature, original.temperature);
        assert_eq!(rewritten.reasoning_effort, original.reasoning_effort);
        assert_eq!(
            format!("{:?}", rewritten.tools),
            format!("{:?}", original.tools)
        );
        assert_eq!(rewritten.tool_choice, original.tool_choice);
        let cached = maybe_compact(&state, original, |_| panic!("must reuse complete prefix"))
            .await
            .unwrap();
        assert_eq!(cached.messages.last().unwrap().text, "CURRENT_TASK");
    }

    #[tokio::test]
    async fn incremental_slice_uses_original_offsets_before_filtering() {
        let state = super::super::state::compact_test_state();
        let mut original = CanonicalRequest::new(Protocol::Responses, "test-model");
        original.messages = vec![
            Message::text(Role::User, format!("{SUMMARY_PREFIX}\nold summary")),
            Message::text(Role::Assistant, "x".repeat(90_000)),
            Message::text(Role::User, "DELTA_USER"),
            Message::text(Role::Assistant, "DELTA_ANSWER"),
            Message::text(Role::User, "CURRENT_TASK"),
        ];
        state
            .summary_cache
            .record(
                &original.model,
                hash_messages(&original.messages, 2),
                2,
                "cached summary".into(),
            )
            .await;
        let rewritten = maybe_compact(&state, original, |summary| {
            Box::pin(async move {
                let texts: Vec<_> = summary.messages.iter().map(|m| m.text.as_str()).collect();
                assert_eq!(
                    texts,
                    vec![
                        "cached summary",
                        "DELTA_USER",
                        "DELTA_ANSWER",
                        SUMMARIZATION_PROMPT
                    ]
                );
                Ok("new summary".into())
            })
        })
        .await
        .unwrap();
        assert_eq!(rewritten.messages.last().unwrap().text, "CURRENT_TASK");
    }

    #[tokio::test]
    async fn failed_or_empty_summary_does_not_poison_cache() {
        let state = super::super::state::compact_test_state();
        let mut original = CanonicalRequest::new(Protocol::Responses, "test-model");
        original.messages = vec![
            Message::text(Role::Assistant, "x".repeat(90_000)),
            Message::text(Role::User, "CURRENT_TASK"),
        ];
        for empty in [false, true] {
            let result = maybe_compact(&state, original.clone(), move |_| {
                Box::pin(async move {
                    if empty {
                        Ok("  ".into())
                    } else {
                        Err(crate::error::GatewayError::Internal("failed".into()))
                    }
                })
            })
            .await;
            assert!(result.is_err());
            assert!(
                state
                    .summary_cache
                    .lookup(&original.model, |n| hash_messages(&original.messages, n), 1)
                    .await
                    .is_none()
            );
        }
    }
}
