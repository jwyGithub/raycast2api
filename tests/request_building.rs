//! Offline tests for the Raycast request builder's id fields (④).
//!
//! These pin the correlation-id semantics the gateway reproduces from a
//! stateless position, as confirmed against a captured real-client session
//! (`charles/think_tools.har`): `thread_id` is constant for a whole conversation
//! (session affinity), `message_id` is constant across the assistant/tool
//! round-trips that resolve one user turn and rotates on a new user message, and
//! `buffer_id` is minted fresh per request. Pure logic — no network involved.

use raycast2api::core::message::{Message, Role};
use raycast2api::core::protocol::Protocol;
use raycast2api::core::request::CanonicalRequest;
use raycast2api::raycast::builder::build_chat_request;

/// A canonical request carrying just `messages` (the ids derive from these).
fn req_with(messages: Vec<Message>) -> CanonicalRequest {
    let mut req = CanonicalRequest::new(Protocol::ChatCompletions, "gpt-x");
    req.messages = messages;
    req
}

#[test]
fn thread_id_is_stable_across_turns() {
    // Turn 1: a single user message.
    let turn1 = req_with(vec![Message::text(Role::User, "hello there")]);
    // Turn 2: same conversation, now with the reply and a follow-up appended.
    let turn2 = req_with(vec![
        Message::text(Role::User, "hello there"),
        Message::text(Role::Assistant, "hi! how can I help?"),
        Message::text(Role::User, "what's the weather?"),
    ]);

    let a = build_chat_request(&turn1, "anthropic", "claude");
    let b = build_chat_request(&turn2, "anthropic", "claude");
    assert_eq!(
        a.thread_id, b.thread_id,
        "same conversation must map to the same thread_id"
    );
}

#[test]
fn thread_id_differs_across_conversations() {
    let a = build_chat_request(
        &req_with(vec![Message::text(Role::User, "topic A")]),
        "anthropic",
        "claude",
    );
    let b = build_chat_request(
        &req_with(vec![Message::text(Role::User, "topic B")]),
        "anthropic",
        "claude",
    );
    assert_ne!(a.thread_id, b.thread_id);
}

#[test]
fn thread_id_falls_back_when_no_user_message() {
    // System-only conversation: no user anchor -> still a valid 26-char id.
    let out = build_chat_request(
        &req_with(vec![Message::text(Role::System, "be nice")]),
        "anthropic",
        "claude",
    );
    assert_eq!(out.thread_id.len(), 26);
}

#[test]
fn buffer_id_is_fresh_per_request() {
    let req = req_with(vec![Message::text(Role::User, "hello there")]);
    let a = build_chat_request(&req, "anthropic", "claude");
    let b = build_chat_request(&req, "anthropic", "claude");
    assert!(a.buffer_id.is_some());
    assert_ne!(a.buffer_id, b.buffer_id, "buffer_id is minted per request");
}

#[test]
fn message_id_is_stable_within_a_user_turn() {
    // Same user turn mid tool-calling: a round-trip appends an assistant tool
    // call and its tool result after the last user message. message_id must not
    // change (matches the captured client behaviour).
    let start = req_with(vec![Message::text(Role::User, "use the tool")]);
    let mid = req_with(vec![
        Message::text(Role::User, "use the tool"),
        Message::text(Role::Assistant, "calling..."),
        Message::text(Role::Tool, "tool result"),
    ]);
    let a = build_chat_request(&start, "anthropic", "claude");
    let b = build_chat_request(&mid, "anthropic", "claude");
    assert_eq!(
        a.message_id, b.message_id,
        "message_id must be stable across tool round-trips of one user turn"
    );
    // It is also deterministic: rebuilding the same input reproduces it.
    let a2 = build_chat_request(&start, "anthropic", "claude");
    assert_eq!(a.message_id, a2.message_id);
}

#[test]
fn message_id_rotates_on_new_user_turn() {
    let turn2 = req_with(vec![Message::text(Role::User, "use the tool")]);
    let turn3 = req_with(vec![
        Message::text(Role::User, "use the tool"),
        Message::text(Role::Assistant, "done"),
        Message::text(Role::User, "now do something else"),
    ]);
    let a = build_chat_request(&turn2, "anthropic", "claude");
    let b = build_chat_request(&turn3, "anthropic", "claude");
    assert_ne!(
        a.message_id, b.message_id,
        "a new user message must start a new message_id"
    );
}

// --- Thinking replay scope ----------------------------------------------------
//
// The upstream content filter re-scans replayed `<think>` text and
// probabilistically blocks whole requests with `content_policy_blocked`
// (observed 2026-11-12/13 on a 452KB transcript; stripping all historical
// thinking blocks let the identical request pass). The builder therefore
// replays thinking only for the CURRENT tool-use loop — messages after the
// last user turn — and drops historical thoughts and their signatures.

fn think_msg(role: Role, text: &str, thought: &str) -> Message {
    let mut m = Message::text(role, text);
    m.thinking = Some(raycast2api::core::message::Thinking {
        text: thought.to_string(),
        signature: Some(format!("sig-for-{thought}")),
    });
    m
}

#[test]
fn historical_thinking_is_not_replayed() {
    // Turn 1 carried thinking; turn 2 is a fresh user message. The old
    // `<think>` turn must vanish from the wire entirely.
    let req = req_with(vec![
        Message::text(Role::User, "q1"),
        think_msg(Role::Assistant, "a1", "old private thought"),
        Message::text(Role::User, "q2"),
    ]);
    let out = build_chat_request(&req, "anthropic", "claude");
    let bodies: Vec<&str> = out
        .messages
        .iter()
        .map(|m| m.content.text.as_str())
        .collect();
    assert_eq!(
        bodies,
        ["q1", "a1", "q2"],
        "no <think> replay for old turns"
    );
}

#[test]
fn no_thinking_is_replayed_anywhere() {
    // Mid tool round-trip: the assistant turn after the last user message is
    // part of the live loop, but its thinking is STILL not replayed — neither
    // official client sends it, and the upstream content filter scans the
    // replayed text regardless of turn position.
    let mut assistant = think_msg(Role::Assistant, "", "current plan");
    assistant.tool_calls = vec![raycast2api::core::message::ToolCall {
        id: "toolu_1".into(),
        name: "Bash".into(),
        arguments: "{}".into(),
    }];
    let req = req_with(vec![
        Message::text(Role::User, "use the tool"),
        assistant,
        Message::text(Role::Tool, "result"),
    ]);
    let out = build_chat_request(&req, "anthropic", "claude");
    // Wire layout: user, assistant (empty text + tool call), tool. No <think>.
    assert_eq!(out.messages.len(), 3);
    assert_eq!(out.messages[0].content.text, "use the tool");
    assert_eq!(out.messages[1].content.text, "");
    assert!(!out.messages[1].tool_calls.is_empty());
    assert_eq!(out.messages[2].content.text, "result");
    assert!(
        !out.messages
            .iter()
            .any(|m| m.content.text.starts_with("<think>")),
        "no <think> replay anywhere on the wire"
    );
}

#[test]
fn historical_signature_is_dropped_with_its_thinking() {
    // A signature belongs to thinking this request no longer replays; sending
    // it would make upstream validate a signature against content it never
    // saw. Old turns carry neither the <think> text nor the signature. The
    // terminal assistant message of the LAST turn keeps its signature so the
    // tool round-trip stays valid.
    let mut current = think_msg(Role::Assistant, "", "fresh thought");
    current.tool_calls = vec![raycast2api::core::message::ToolCall {
        id: "toolu_2".into(),
        name: "Bash".into(),
        arguments: "{}".into(),
    }];
    let req = req_with(vec![
        Message::text(Role::User, "q1"),
        think_msg(Role::Assistant, "a1", "old thought"),
        Message::text(Role::User, "q2"),
        current,
        Message::text(Role::Tool, "result"),
    ]);
    let out = build_chat_request(&req, "anthropic", "claude");
    // Wire layout: q1, a1, q2, ""+tool call, result — no <think> anywhere.
    let historical = &out.messages[1]; // the "a1" assistant turn
    assert!(historical.extra_content.is_none(), "old signature dropped");
    // Terminal assistant message of the last turn carries the signature (it
    // ends the turn in a tool call).
    let terminal = &out.messages[3];
    assert_eq!(terminal.content.text, "");
    assert!(!terminal.tool_calls.is_empty());
    assert!(
        terminal.extra_content.is_some(),
        "last-turn signature preserved"
    );
}

#[test]
fn user_text_adjacent_to_tool_results_does_not_reset_signature() {
    // The regression that failed in production: a client-adjacent user text
    // like "Tool loaded." emitted AFTER tool results made the previous
    // last-user gate treat later thinking as "current loop". Under the
    // all-strip rule no thinking is replayed at all, so this shape stays
    // clean regardless, and the signature lands on the final assistant
    // message of the transcript.
    let mut assistant = think_msg(Role::Assistant, "working", "loop thought");
    assistant.tool_calls = vec![raycast2api::core::message::ToolCall {
        id: "toolu_3".into(),
        name: "ToolSearch".into(),
        arguments: "{}".into(),
    }];
    let req = req_with(vec![
        Message::text(Role::User, "q1"),
        assistant,
        Message::text(Role::Tool, "Tool loaded."),
        Message::text(Role::User, "Tool loaded."),
        Message::text(Role::Tool, "result"),
    ]);
    let out = build_chat_request(&req, "anthropic", "claude");
    assert!(
        !out.messages
            .iter()
            .any(|m| m.content.text.starts_with("<think>")),
        "tool-adjacent user text must not resurrect thinking replay"
    );
    // Signature rides on the last assistant message ("working"), tool-call
    // turn or not.
    let last_assistant = out
        .messages
        .iter()
        .rposition(|m| m.role == "assistant")
        .expect("assistant present");
    assert!(out.messages[last_assistant].extra_content.is_some());
}
