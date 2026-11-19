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
//! Redis shares both caches across restarts; bounded local caches remain the
//! fallback, and each remote operation has its own short timeout.

mod cache;
mod plan;

pub use cache::CompactCache;
pub(crate) use cache::UsageKey;
pub(crate) use plan::{Compaction, apply, plan, summarize_messages};
pub use plan::{SUMMARIZATION_PROMPT, SUMMARY_PREFIX};

#[cfg(test)]
mod tests;
