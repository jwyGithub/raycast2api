//! Offline tests pinning the JSON shape of the OpenAI-compatible contract.
//!
//! The gateway's whole point is being drop-in compatible with OpenAI clients,
//! so these lock the serialized field names/values for the non-streaming
//! response and the streaming chunks, plus the deserialization defaults for
//! incoming requests. Pure serde — no network.

use raycast2api::protocol::chat_completions::wire::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
};
use serde_json::json;

#[test]
fn non_streaming_response_shape() {
    let resp = ChatCompletionResponse::from_text(
        "chatcmpl-abc".to_string(),
        1_700_000_000,
        "openai-gpt-4o-mini".to_string(),
        "hello world".to_string(),
    );
    let value = serde_json::to_value(&resp).unwrap();

    assert_eq!(value["object"], "chat.completion");
    assert_eq!(value["id"], "chatcmpl-abc");
    assert_eq!(value["created"], 1_700_000_000u64);
    assert_eq!(value["model"], "openai-gpt-4o-mini");
    assert_eq!(value["choices"][0]["index"], 0);
    assert_eq!(value["choices"][0]["finish_reason"], "stop");
    assert_eq!(value["choices"][0]["message"]["role"], "assistant");
    assert_eq!(value["choices"][0]["message"]["content"], "hello world");
    assert_eq!(
        value["usage"],
        json!({ "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 })
    );
}

#[test]
fn first_delta_chunk_includes_role_and_content() {
    let chunk =
        ChatCompletionChunk::delta("chatcmpl-1", 1, "m", Some("assistant"), Some("Hi".into()));
    let value = serde_json::to_value(&chunk).unwrap();

    assert_eq!(value["object"], "chat.completion.chunk");
    assert_eq!(value["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(value["choices"][0]["delta"]["content"], "Hi");
    // Non-terminal chunks carry a null finish_reason.
    assert!(value["choices"][0]["finish_reason"].is_null());
}

#[test]
fn empty_delta_omits_role_and_content_fields() {
    // role=None, content=None → the delta must serialize to an empty object so
    // we never emit `"role":null` / `"content":null` to OpenAI clients.
    let chunk = ChatCompletionChunk::delta("id", 1, "m", None, None);
    let value = serde_json::to_value(&chunk).unwrap();
    assert_eq!(value["choices"][0]["delta"], json!({}));
}

#[test]
fn stop_chunk_sets_finish_reason_and_empty_delta() {
    let chunk = ChatCompletionChunk::stop("id", 1, "m");
    let value = serde_json::to_value(&chunk).unwrap();
    assert_eq!(value["choices"][0]["finish_reason"], "stop");
    assert_eq!(value["choices"][0]["delta"], json!({}));
}

#[test]
fn request_defaults_stream_false_and_model_none() {
    let req: ChatCompletionRequest =
        serde_json::from_str(r#"{"messages":[{"role":"user","content":"hi"}]}"#).unwrap();

    assert!(!req.stream);
    assert!(req.model.is_none());
    assert!(req.temperature.is_none());
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].role, "user");
    assert_eq!(req.messages[0].content, "hi");
}

#[test]
fn request_ignores_unknown_fields() {
    let req: ChatCompletionRequest =
        serde_json::from_str(r#"{"messages":[],"model":"gpt-4","stream":true,"top_p":0.9,"n":3}"#)
            .unwrap();

    assert!(req.stream);
    assert_eq!(req.model.as_deref(), Some("gpt-4"));
    assert!(req.messages.is_empty());
}

#[test]
fn tool_call_response_sets_finish_reason_and_null_content() {
    use raycast2api::protocol::chat_completions::wire::{ToolCall, Usage};
    let resp = ChatCompletionResponse::new(
        "chatcmpl-t".to_string(),
        1,
        "m".to_string(),
        None,
        vec![ToolCall::function(
            "call_1".into(),
            "get_weather".into(),
            r#"{"city":"SF"}"#.into(),
        )],
        "tool_calls",
        Usage::default(),
    );
    let value = serde_json::to_value(&resp).unwrap();
    assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
    assert!(value["choices"][0]["message"]["content"].is_null());
    let tc = &value["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(tc["id"], "call_1");
    assert_eq!(tc["type"], "function");
    assert_eq!(tc["function"]["name"], "get_weather");
    assert_eq!(tc["function"]["arguments"], r#"{"city":"SF"}"#);
}

#[test]
fn usage_maps_cached_tokens_into_details() {
    use raycast2api::protocol::chat_completions::wire::{PromptTokensDetails, Usage};
    let resp = ChatCompletionResponse::new(
        "id".into(),
        1,
        "m".into(),
        Some("hi".into()),
        vec![],
        "stop",
        Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 30 }),
        },
    );
    let value = serde_json::to_value(&resp).unwrap();
    assert_eq!(value["usage"]["prompt_tokens"], 100);
    assert_eq!(value["usage"]["completion_tokens"], 20);
    assert_eq!(value["usage"]["total_tokens"], 120);
    assert_eq!(value["usage"]["prompt_tokens_details"]["cached_tokens"], 30);
}

#[test]
fn text_response_keeps_original_three_field_usage() {
    // The plain-text path must keep the original usage object (no details) and
    // must not emit a tool_calls field.
    let resp = ChatCompletionResponse::from_text("id".into(), 1, "m".into(), "hi".into());
    let value = serde_json::to_value(&resp).unwrap();
    assert!(value["usage"].get("prompt_tokens_details").is_none());
    assert!(value["choices"][0]["message"].get("tool_calls").is_none());
}

#[test]
fn streaming_tool_call_delta_shape() {
    let chunk = ChatCompletionChunk::tool_call_delta(
        "id",
        1,
        "m",
        Some("assistant"),
        0,
        "call_9".into(),
        "search".into(),
        r#"{"q":"x"}"#.into(),
    );
    let value = serde_json::to_value(&chunk).unwrap();
    let delta = &value["choices"][0]["delta"];
    assert_eq!(delta["role"], "assistant");
    assert!(delta.get("content").is_none());
    let tc = &delta["tool_calls"][0];
    assert_eq!(tc["index"], 0);
    assert_eq!(tc["id"], "call_9");
    assert_eq!(tc["type"], "function");
    assert_eq!(tc["function"]["name"], "search");
    assert_eq!(tc["function"]["arguments"], r#"{"q":"x"}"#);
    assert!(value["choices"][0]["finish_reason"].is_null());
}

#[test]
fn finish_chunk_maps_tool_calls_reason() {
    let chunk = ChatCompletionChunk::finish("id", 1, "m", "tool_calls");
    let value = serde_json::to_value(&chunk).unwrap();
    assert_eq!(value["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(value["choices"][0]["delta"], json!({}));
}

#[test]
fn finish_chunk_omits_usage() {
    // The terminating chunk carries no usage; usage rides on its own trailing
    // chunk. Every non-usage chunk must stay byte-identical (no `usage` key).
    let chunk = ChatCompletionChunk::stop("id", 1, "m");
    let value = serde_json::to_value(&chunk).unwrap();
    assert!(value.get("usage").is_none());
}

#[test]
fn usage_chunk_has_empty_choices_and_full_usage() {
    use raycast2api::protocol::chat_completions::wire::{PromptTokensDetails, Usage};
    let chunk = ChatCompletionChunk::usage(
        "chatcmpl-1",
        1,
        "m",
        Usage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            prompt_tokens_details: Some(PromptTokensDetails { cached_tokens: 30 }),
        },
    );
    let value = serde_json::to_value(&chunk).unwrap();

    assert_eq!(value["object"], "chat.completion.chunk");
    // The trailing usage chunk carries no choices.
    assert_eq!(value["choices"], json!([]));
    assert_eq!(value["usage"]["prompt_tokens"], 100);
    assert_eq!(value["usage"]["completion_tokens"], 20);
    assert_eq!(value["usage"]["total_tokens"], 120);
    assert_eq!(value["usage"]["prompt_tokens_details"]["cached_tokens"], 30);
}
