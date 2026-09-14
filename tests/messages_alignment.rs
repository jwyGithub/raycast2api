//! Alignment tests for the Anthropic **Messages** path against real Claude Code
//! traffic (captured in `agent/anthropic/conversation-ad7b00d114b0.json`).
//!
//! These pin the six behaviours the gateway was drifting from the real client
//! on. Each test drives the full inbound → Raycast-builder path so it asserts
//! the bytes that actually reach the backend:
//!
//! 1. `system`-role turns inside `messages[]` are kept (as `user`), not dropped.
//! 2. `system_instructions` stays Raycast's base (`"markdown"`) while the
//!    client's `system` prompt rides along as `additional_system_instructions`.
//! 3. `thinking.type == "adaptive"` turns reasoning on (→ `reasoning_effort`).
//! 4. a `tool_result` message carries the tool `name`, resolved from the
//!    matching earlier `tool_use`.
//! 5. `tool_choice` defaults to `"auto"` when tools are offered but unspecified.
//! 6. multiple `text` blocks in one message join with `"\n"`.
//!
//! Pure serde + the pure builder — no network.

use raycast2api::core::message::Role;
use raycast2api::core::request::{CanonicalRequest, ReasoningEffort};
use raycast2api::protocol::InboundAdapter;
use raycast2api::protocol::messages::MessagesInbound;
use raycast2api::protocol::messages::wire::MessagesRequest;
use raycast2api::raycast::builder::build_chat_request;
use serde_json::{Value, json};

/// Deserialize a `/v1/messages` body and run the inbound adapter.
fn parse(body: Value) -> CanonicalRequest {
    let request: MessagesRequest = serde_json::from_value(body).unwrap();
    MessagesInbound::parse(request).expect("valid request")
}

// --- Fix #1: system-role conversation turns are preserved as user turns ------

#[test]
fn system_role_messages_are_kept_as_user_turns() {
    // Claude Code injects context as `system`-role turns *inside* messages[].
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [
            { "role": "user", "content": "hi" },
            { "role": "system", "content": "Available agent types: ..." },
            { "role": "assistant", "content": "ok" },
            { "role": "system", "content": [ { "type": "text", "text": "env note" } ] }
        ]
    }));

    // Nothing is dropped; the two system turns become user turns, in place.
    let roles: Vec<Role> = canonical.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [Role::User, Role::User, Role::Assistant, Role::User]);
    assert_eq!(canonical.messages[1].text, "Available agent types: ...");
    assert_eq!(canonical.messages[3].text, "env note");
}

#[test]
fn genuinely_unknown_roles_are_still_dropped() {
    // Only `system` is rescued; other unknown roles remain dropped.
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [
            { "role": "user", "content": "hi" },
            { "role": "developer", "content": "ignored" }
        ]
    }));

    assert_eq!(canonical.messages.len(), 1);
    assert_eq!(canonical.messages[0].role, Role::User);
}

// --- Fix #2: system prompt routing -------------------------------------------

#[test]
fn client_system_prompt_rides_as_additional_instructions() {
    let canonical = parse(json!({
        "model": "claude-x",
        "system": "You are Claude Code.",
        "messages": [ { "role": "user", "content": "hi" } ]
    }));

    let raycast = build_chat_request(&canonical, "anthropic", "claude");
    // Base prompt stays "markdown"; the client's system becomes the addendum.
    assert_eq!(raycast.system_instructions, "markdown");
    assert_eq!(
        raycast.additional_system_instructions.as_deref(),
        Some("You are Claude Code.")
    );
}

#[test]
fn absent_system_leaves_additional_unset() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [ { "role": "user", "content": "hi" } ]
    }));

    let raycast = build_chat_request(&canonical, "anthropic", "claude");
    assert_eq!(raycast.system_instructions, "markdown");
    // No client system → the addendum is omitted entirely (serde skips None).
    assert!(raycast.additional_system_instructions.is_none());
    let value = serde_json::to_value(&raycast).unwrap();
    assert!(value.get("additional_system_instructions").is_none());
}

// --- Fix #3: adaptive thinking enables reasoning -----------------------------

#[test]
fn adaptive_thinking_enables_reasoning() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [ { "role": "user", "content": "hi" } ],
        "thinking": { "type": "adaptive" }
    }));

    assert!(matches!(
        canonical.reasoning_effort,
        Some(ReasoningEffort::Medium)
    ));

    let raycast = build_chat_request(&canonical, "anthropic", "claude");
    assert_eq!(raycast.reasoning_effort.as_deref(), Some("medium"));
}

#[test]
fn disabled_thinking_leaves_reasoning_off() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [ { "role": "user", "content": "hi" } ],
        "thinking": { "type": "disabled" }
    }));
    assert!(canonical.reasoning_effort.is_none());
}

// --- Fix #4: tool_result messages carry the resolved tool name ---------------

#[test]
fn tool_result_is_labelled_with_its_tool_name() {
    // assistant calls `Read` (id call_1), then the user returns its result.
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [
            { "role": "user", "content": "read the file" },
            {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "" },
                    { "type": "tool_use", "id": "call_1", "name": "Read", "input": { "path": "a.txt" } }
                ]
            },
            {
                "role": "user",
                "content": [ { "type": "tool_result", "tool_use_id": "call_1", "content": "file body" } ]
            }
        ]
    }));

    let tool = canonical
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("a tool message");
    assert_eq!(tool.tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(tool.tool_name.as_deref(), Some("Read"));

    // And it survives to the Raycast wire as `name`.
    let raycast = build_chat_request(&canonical, "anthropic", "claude");
    let wire_tool = raycast
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("a wire tool message");
    assert_eq!(wire_tool.name.as_deref(), Some("Read"));
    assert_eq!(wire_tool.tool_call_id.as_deref(), Some("call_1"));
}

// --- Fix #5: tool_choice defaults to auto when tools are present -------------

#[test]
fn tool_choice_defaults_to_auto_when_tools_present() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [ { "role": "user", "content": "hi" } ],
        "tools": [ { "name": "Read", "input_schema": { "type": "object" } } ]
        // no tool_choice
    }));
    // The client sent none...
    assert!(canonical.tool_choice.is_none());

    // ...but the builder defaults it to "auto" because tools are offered.
    let raycast = build_chat_request(&canonical, "anthropic", "claude");
    assert_eq!(raycast.tool_choice.as_deref(), Some("auto"));
}

#[test]
fn tool_choice_stays_unset_without_tools() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [ { "role": "user", "content": "hi" } ]
    }));
    let raycast = build_chat_request(&canonical, "anthropic", "claude");
    assert!(raycast.tool_choice.is_none());
}

// --- Fix #6: multiple text blocks join with a newline ------------------------

#[test]
fn multiple_user_text_blocks_join_with_newline() {
    let canonical = parse(json!({
        "model": "claude-x",
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "first" },
                { "type": "text", "text": "second" }
            ]
        }]
    }));

    assert_eq!(canonical.messages.len(), 1);
    assert_eq!(canonical.messages[0].text, "first\nsecond");
}
