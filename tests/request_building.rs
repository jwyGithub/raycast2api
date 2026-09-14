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
