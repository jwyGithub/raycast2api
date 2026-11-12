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
//! 1. **Estimate** request tokens (chars/4, mirroring Codex's
//!    `approx_token_count`) against the model's `context` (in K tokens from
//!    `models.json`). Compaction triggers when usage crosses
//!    [`AUTO_COMPACT_RATIO`] (90%, Codex's default) of the window.
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

use std::collections::HashMap;
use std::time::{Duration, Instant};

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

/// Safety margin subtracted from the estimated request tokens before
/// comparing against the trigger threshold, absorbing estimator error and the
/// completion itself.
const TRIGGER_MARGIN_TOKENS: u64 = 8_000;

/// How long a cached summary stays valid.
const SUMMARY_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

/// The verbatim Codex summarization template
/// (`codex-rs/prompts/templates/compact/prompt.md`).
pub const SUMMARIZATION_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.\n\nInclude:\n- Current progress and key decisions made\n- Important context, constraints, or user preferences\n- What remains to be done (clear next steps)\n- Any critical data, examples, or references needed to continue\n\nBe concise, structured, and focused on helping the next LLM seamlessly continue the work.";

/// The fixed prefix marking a compaction summary (Codex:
/// `SUMMARY_PREFIX`), so `is_summary` can recognize prior summaries.
pub const SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// Rough token estimate: ~4 chars per token (Codex's `approx_token_count`).
fn approx_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
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
        let list = entries.entry(model.to_string()).or_default();
        let now = Instant::now();
        list.retain(|c| now.duration_since(c.created_at) < SUMMARY_CACHE_TTL);
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
            // The summarization prompt replaces the client's system prompt and
            // tools; the summary call is a plain completion.
            let summary_request = CanonicalRequest {
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
            let summary_text = format!("{SUMMARY_PREFIX}\n{summary}");
            let messages = rebuild(&request.messages, &summary_text);
            tracing::info!(
                model = %request.model,
                summary_chars = summary.len(),
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
    // `context` is in K tokens (models.json); lives in the passthrough `extra`.
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

    let request_tokens = estimate_request_tokens(request) as u64;
    let threshold = (window as f64 * AUTO_COMPACT_RATIO) as u64 - TRIGGER_MARGIN_TOKENS;
    if request_tokens < threshold {
        return Compaction::NotNeeded;
    }
    tracing::info!(
        model = %request.model,
        estimated_tokens = request_tokens,
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
    let mut chars: usize = 0;
    if let Some(system) = &request.system {
        chars += system.len();
    }
    for message in &request.messages {
        chars += message.text.len();
        for call in &message.tool_calls {
            chars += call.arguments.len() + call.name.len();
        }
    }
    for tool in &request.tools {
        // Schema shape via Debug formatting — ToolDef doesn't impl Serialize,
        // and an approximate footprint is all the estimator needs.
        chars += format!("{tool:?}").len();
    }
    chars / 4
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
