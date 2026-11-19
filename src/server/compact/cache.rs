//! Compaction caches: measured input-token usage and generated summaries.
//!
//! Both are bounded in-process caches (L1) keyed by prompt-prefix hashes, with
//! an optional Redis second level shared across restarts. Remote operations
//! are best-effort and bounded by [`RedisKv`]'s short timeout.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::core::message::Message;
use crate::core::request::CanonicalRequest;
use crate::storage::redis_store::RedisKv;

use super::plan::SUMMARY_PREFIX;

/// How long a cached summary stays valid.
pub(super) const SUMMARY_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
pub(super) const USAGE_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
pub(super) const USAGE_CACHE_CAPACITY: usize = 1_024;

pub(super) fn redis_cache_key(redis: &RedisKv, kind: &str, hash: &[u8; 32]) -> String {
    redis.key(&format!("compact:v1:{kind}"), &hex::encode(hash))
}

/// Identifies the exact input whose token count arrives at stream completion.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct UsageKey {
    pub(super) prefix_hash: [u8; 32],
    pub(super) covered_messages: usize,
}

impl UsageKey {
    pub(crate) fn new(request: &CanonicalRequest) -> Self {
        Self {
            prefix_hash: *prompt_prefix_hashes(request).last().unwrap(),
            covered_messages: request.messages.len(),
        }
    }
}

pub(super) struct CachedUsage {
    pub(super) key: UsageKey,
    pub(super) input_tokens: u32,
    pub(super) created_at: Instant,
}

/// Bounded input-token measurements; stores hashes rather than conversation text.
#[derive(Default)]
pub struct UsageCache {
    pub(super) entries: Mutex<VecDeque<CachedUsage>>,
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

    pub(super) async fn lookup(&self, hashes: &[[u8; 32]]) -> Option<(usize, u32)> {
        let entries = self.entries.lock().await;
        let now = Instant::now();
        let entry = entries
            .iter()
            .filter(|entry| {
                now.duration_since(entry.created_at) < USAGE_CACHE_TTL
                    && hashes.get(entry.key.covered_messages) == Some(&entry.key.prefix_hash)
            })
            .max_by_key(|entry| entry.key.covered_messages)?;
        Some((entry.key.covered_messages, entry.input_tokens))
    }

    #[cfg(test)]
    pub(crate) async fn estimate_tokens(&self, request: &CanonicalRequest) -> Option<u64> {
        self.lookup(&prompt_prefix_hashes(request))
            .await
            .map(|entry| tokens_with_suffix(request, entry))
    }
}

fn tokens_with_suffix(request: &CanonicalRequest, (covered, tokens): (usize, u32)) -> u64 {
    u64::from(tokens) + (message_bytes(&request.messages[covered..]) as u64).div_ceil(4)
}

/// Local (L1) caches plus the optional Redis (L2) used by auto-compact.
/// Owned by `AppState`; compaction logic only depends on this, not on `AppState`.
#[derive(Default)]
pub struct CompactCache {
    pub(super) usage: UsageCache,
    pub(super) summary: SummaryCache,
    pub(super) redis: Option<RedisKv>,
}

impl CompactCache {
    #[cfg(test)]
    pub(crate) async fn estimate_tokens(&self, request: &CanonicalRequest) -> Option<u64> {
        self.usage.estimate_tokens(request).await
    }

    pub fn with_redis(mut self, redis: Option<RedisKv>) -> Self {
        self.redis = redis;
        self
    }

    async fn redis_cached_prefixes(&self, kind: &str, hashes: &[[u8; 32]]) -> Vec<Option<Vec<u8>>> {
        let Some(redis) = self.redis.as_ref().filter(|_| !hashes.is_empty()) else {
            return Vec::new();
        };
        let keys = hashes
            .iter()
            .map(|hash| redis_cache_key(redis, kind, hash))
            .collect();
        redis.mget(keys).await.unwrap_or_else(|| {
            tracing::warn!(
                kind,
                "compact: Redis cache read unavailable; using local estimate/cache"
            );
            Vec::new()
        })
    }

    async fn redis_record(
        &self,
        kind: &str,
        hash: &[u8; 32],
        value: impl redis::ToRedisArgs,
        ttl: Duration,
    ) {
        let Some(redis) = self.redis.as_ref() else {
            return;
        };
        if !redis
            .set_ex(redis_cache_key(redis, kind, hash), value, ttl)
            .await
        {
            tracing::warn!(
                kind,
                "compact: Redis cache write unavailable; kept local entry"
            );
        }
    }

    pub(crate) async fn record_usage(&self, key: UsageKey, input_tokens: u32) {
        if input_tokens == 0 || key.covered_messages == 0 {
            return;
        }
        self.usage.record(key, input_tokens).await;
        self.redis_record("usage", &key.prefix_hash, input_tokens, USAGE_CACHE_TTL)
            .await;
    }

    pub(super) async fn estimate_input_tokens(&self, request: &CanonicalRequest) -> Option<u64> {
        let hashes = prompt_prefix_hashes(request);
        let local = self.usage.lookup(&hashes).await;
        let start = local.map_or(1, |(covered, _)| covered + 1);
        let remote = self
            .redis_cached_prefixes("usage", &hashes[start..])
            .await
            .into_iter()
            .enumerate()
            .rev()
            .find_map(|(index, value)| {
                let tokens = std::str::from_utf8(value.as_deref()?)
                    .ok()?
                    .parse::<u32>()
                    .ok()?;
                (tokens > 0).then_some((start + index, tokens))
            });
        remote
            .or(local)
            .map(|entry| tokens_with_suffix(request, entry))
    }

    pub(super) async fn record_summary(&self, request: &CanonicalRequest, summary: String) {
        let covered = request.messages.len().saturating_sub(1);
        if covered == 0 || summary.trim().is_empty() {
            return;
        }
        let hash = prompt_prefix_hashes(request)[covered];
        self.summary
            .record(&request.model, hash, covered, summary.clone())
            .await;
        self.redis_record("summary", &hash, summary, SUMMARY_CACHE_TTL)
            .await;
    }

    pub(super) async fn cached_summary(
        &self,
        request: &CanonicalRequest,
    ) -> Option<(usize, String)> {
        let total = request.messages.len().saturating_sub(1);
        let hashes = prompt_prefix_hashes(request);
        let local = self.summary.lookup(&request.model, &hashes[..=total]).await;
        let start = local.as_ref().map_or(1, |(covered, _)| covered + 1);
        if start > total {
            return local;
        }
        let remote = self
            .redis_cached_prefixes("summary", &hashes[start..=total])
            .await
            .into_iter()
            .enumerate()
            .rev()
            .find_map(|(index, value)| {
                let summary = String::from_utf8(value?).ok()?;
                let body = summary.strip_prefix(SUMMARY_PREFIX)?.strip_prefix('\n')?;
                (!body.trim().is_empty()).then_some((start + index, summary))
            });
        // Remote reads do not populate L1 or extend the original Redis TTL.
        remote.or(local)
    }
}

/// Every prefix includes the prompt configuration, so unrelated inputs never
/// share measurements merely because they use the same model or first message.
pub(super) fn prompt_prefix_hashes(request: &CanonicalRequest) -> Vec<[u8; 32]> {
    // Bump compact:v1 when changing the serialized input or compaction recipe.
    let mut hash = Sha256::new();
    hash.update(
        serde_json::to_vec(&(
            &request.model,
            &request.system,
            &request.tools,
            &request.tool_choice,
            request.reasoning_effort.map(|effort| effort.as_str()),
            &request.requested_effort,
            &request.requested_thinking,
        ))
        .expect("cache prompt fields are JSON serializable"),
    );
    let mut hashes = Vec::with_capacity(request.messages.len() + 1);
    hashes.push(hash.clone().finalize().into());
    for message in &request.messages {
        // The builder never replays thinking text; clients may omit it next turn.
        hash.update(
            serde_json::to_vec(&(
                message.role.as_str(),
                &message.text,
                &message.tool_calls,
                &message.tool_call_id,
                &message.tool_name,
                message.tool_error,
            ))
            .expect("cache message fields are JSON serializable"),
        );
        hashes.push(hash.clone().finalize().into());
    }
    hashes
}

/// A cached summary keyed by the hash of the dropped prefix it covers.
pub(super) struct CachedSummary {
    pub(super) prefix_hash: [u8; 32],
    /// Number of leading messages the summary covers.
    pub(super) covered_messages: usize,
    pub(super) summary_text: String,
    pub(super) created_at: Instant,
}

/// Per-model-id summary cache shared across requests.
#[derive(Default)]
pub struct SummaryCache {
    pub(super) entries: Mutex<HashMap<String, Vec<CachedSummary>>>,
}

impl SummaryCache {
    /// Record that `summary_text` summarizes the first `covered_messages`
    /// messages of the conversation hashed as `prefix_hash` under `model`.
    pub(super) async fn record(
        &self,
        model: &str,
        prefix_hash: [u8; 32],
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
        list.retain(|c| c.prefix_hash != prefix_hash);
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
    pub(super) async fn lookup(&self, model: &str, hashes: &[[u8; 32]]) -> Option<(usize, String)> {
        let entries = self.entries.lock().await;
        let list = entries.get(model)?;
        let now = Instant::now();
        list.iter()
            .filter(|c| {
                now.duration_since(c.created_at) < SUMMARY_CACHE_TTL
                    && hashes.get(c.covered_messages) == Some(&c.prefix_hash)
            })
            // Longest cover wins: the least leftover work for the delta pass.
            .max_by_key(|c| c.covered_messages)
            .map(|c| (c.covered_messages, c.summary_text.clone()))
    }
}

pub(super) fn message_bytes(messages: &[Message]) -> usize {
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
