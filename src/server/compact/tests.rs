use super::cache::*;
use super::plan::*;
use super::*;
use crate::core::message::{Message, Role};
use crate::core::request::CanonicalRequest;
use crate::server::state::SharedState;
use std::time::{Duration, Instant};

async fn plan_for_test(state: &SharedState, request: &CanonicalRequest) -> Compaction {
    match state.resolve_model(&request.model).await {
        Ok(model) => plan(&state.compact, request, &model).await,
        Err(_) => Compaction::NotNeeded,
    }
}

/// Inspect an incoming request and, when the conversation has outgrown the
/// model's context window, rewrite it into the compacted form. `send` runs a
/// model completion (used for the summarization call itself).
async fn maybe_compact(
    state: &SharedState,
    request: CanonicalRequest,
    send: impl FnOnce(
        CanonicalRequest,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<String, crate::error::GatewayError>,
    >,
) -> Result<CanonicalRequest, crate::error::GatewayError> {
    let plan = plan_for_test(state, &request).await;
    apply(&state.compact, request, plan, send).await
}

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

#[tokio::test]
async fn plan_applies_model_aliases_before_model_lookup() {
    let state = crate::server::state::compact_test_state();
    state.aliases.create("alias", "test-model").unwrap();

    let build = |model: &str| {
        let mut request = CanonicalRequest::new(Protocol::Responses, model);
        request.messages = vec![
            Message::text(Role::Assistant, "x".repeat(90_000)),
            Message::text(Role::User, "CURRENT_TASK"),
        ];
        request
    };
    // The alias must reach the same model entry (and its context window)
    // as the real id; an unknown id still skips compaction.
    for model in ["test-model", "alias"] {
        assert!(
            matches!(
                plan_for_test(&state, &build(model)).await,
                Compaction::Summarize(_)
            ),
            "{model} should trigger compaction"
        );
    }
    assert!(matches!(
        plan_for_test(&state, &build("unknown")).await,
        Compaction::NotNeeded
    ));
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
    let mut request = request(filler_messages(10, 100));
    let h5 = prompt_prefix_hashes(&request)[5];
    request.messages.push(Message::text(Role::User, "more"));
    // Appending later messages must not change the prefix hash.
    assert_eq!(prompt_prefix_hashes(&request)[5], h5);
    assert_ne!(prompt_prefix_hashes(&request)[6], h5);
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
    let summaries = SummaryCache::default();
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
    summaries
        .record(
            &original.model,
            UsageKey::new(&original).prefix_hash,
            3,
            "summary".into(),
        )
        .await;
    for field in [
        "model",
        "system",
        "tools",
        "tool_choice",
        "effort",
        "requested_effort",
        "requested_thinking",
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
            "requested_effort" => changed.requested_effort = Some("high".into()),
            "requested_thinking" => {
                changed.requested_thinking = Some(serde_json::json!({"type": "adaptive"}))
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
        assert_eq!(
            summaries
                .lookup(&changed.model, &prompt_prefix_hashes(&changed))
                .await,
            None,
            "{field}"
        );
    }
    // Changes the builder does not replay must not invalidate input usage.
    original.messages[1].thinking = Some(crate::core::message::Thinking {
        text: "private thinking".into(),
        signature: None,
    });
    original.stream = true;
    assert_eq!(cache.estimate_tokens(&original).await, Some(1000));
}

#[test]
fn prefix_hashes_preserve_message_and_field_boundaries() {
    let first = request(vec![
        Message::text(Role::User, "aassistant"),
        Message::text(Role::Assistant, "b"),
    ]);
    let second = request(vec![
        Message::text(Role::User, "a"),
        Message::text(Role::Assistant, "assistantb"),
    ]);
    assert_ne!(
        prompt_prefix_hashes(&first)[2],
        prompt_prefix_hashes(&second)[2]
    );
    let mut tool_a = request(vec![Message::text(Role::Assistant, "")]);
    tool_a.messages[0]
        .tool_calls
        .push(crate::core::message::ToolCall {
            id: "ab".into(),
            name: "c".into(),
            arguments: "{}".into(),
        });
    let mut tool_b = tool_a.clone();
    tool_b.messages[0].tool_calls[0].id = "a".into();
    tool_b.messages[0].tool_calls[0].name = "bc".into();
    assert_ne!(
        prompt_prefix_hashes(&tool_a)[1],
        prompt_prefix_hashes(&tool_b)[1]
    );
}

#[tokio::test]
async fn summary_cache_deduplicates_expires_and_stays_bounded() {
    let cache = SummaryCache::default();
    let mut original = request(vec![Message::text(Role::User, "seed")]);
    let hash = prompt_prefix_hashes(&original)[1];
    cache.record(&original.model, hash, 1, "old".into()).await;
    cache.record(&original.model, hash, 1, "new".into()).await;
    assert_eq!(cache.entries.lock().await[&original.model].len(), 1);
    assert_eq!(
        cache
            .lookup(&original.model, &prompt_prefix_hashes(&original))
            .await,
        Some((1, "new".into()))
    );
    cache.entries.lock().await.get_mut(&original.model).unwrap()[0].created_at =
        Instant::now() - SUMMARY_CACHE_TTL;
    assert_eq!(
        cache
            .lookup(&original.model, &prompt_prefix_hashes(&original))
            .await,
        None
    );
    for i in 0..=128 {
        original.messages[0].text = i.to_string();
        cache
            .record(
                &original.model,
                prompt_prefix_hashes(&original)[1],
                1,
                "summary".into(),
            )
            .await;
    }
    assert_eq!(cache.entries.lock().await[&original.model].len(), 128);
    for i in 0..=32 {
        cache
            .record(&i.to_string(), hash, 1, "summary".into())
            .await;
    }
    assert_eq!(cache.entries.lock().await.len(), 32);
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
            .compact
            .usage
            .record(UsageKey::new(&original), measured)
            .await;
        original.messages.extend([
            Message::text(Role::Assistant, "done"),
            Message::text(Role::User, "next"),
        ]);
        assert_eq!(
            matches!(
                plan_for_test(&state, &original).await,
                Compaction::Summarize(_)
            ),
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
        .compact
        .usage
        .record(UsageKey::new(&compacted), 100)
        .await;
    assert_eq!(state.compact.usage.estimate_tokens(&original).await, None);
    original.messages.extend([
        Message::text(Role::Assistant, "done"),
        Message::text(Role::User, "NEXT_TASK"),
    ]);
    assert!(matches!(
        plan_for_test(&state, &original).await,
        Compaction::Summarize(_)
    ));
    let mut continued = compacted;
    continued.messages.extend([
        Message::text(Role::Assistant, "done"),
        Message::text(Role::User, "next"),
    ]);
    assert_eq!(
        state.compact.usage.estimate_tokens(&continued).await,
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
        .compact
        .summary
        .record(
            &original.model,
            prompt_prefix_hashes(&original)[2],
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
        assert!(state.compact.cached_summary(&original).await.is_none());
    }
}

#[path = "../../../tests/unit/compact_redis.rs"]
mod redis_tests;
