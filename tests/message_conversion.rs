//! Offline tests for the OpenAI inbound mapping (`ChatCompletionsInbound::parse`).
//!
//! These pin the mapping rules the gateway depends on: a *leading* `system`
//! message becomes the canonical system prompt, `user`/`assistant` messages are
//! forwarded verbatim and in order, and every other message is dropped. The
//! `"markdown"` default for an absent system prompt now lives in the Raycast
//! builder, so one case asserts it there. Pure logic — no network involved.

use raycast2api::core::message::Role;
use raycast2api::protocol::InboundAdapter;
use raycast2api::protocol::chat_completions::ChatCompletionsInbound;
use raycast2api::protocol::chat_completions::wire::{ChatCompletionRequest, ChatMessage};
use raycast2api::raycast::builder::build_chat_request;

fn msg(role: &str, content: &str) -> ChatMessage {
    ChatMessage {
        role: role.to_string(),
        content: content.to_string(),
    }
}

fn request(messages: Vec<ChatMessage>) -> ChatCompletionRequest {
    ChatCompletionRequest {
        messages,
        model: None,
        temperature: None,
        stream: false,
    }
}

#[test]
fn leading_system_message_becomes_system_prompt() {
    let canonical = ChatCompletionsInbound::parse(request(vec![
        msg("system", "be terse"),
        msg("user", "hello"),
    ]))
    .expect("valid request");

    assert_eq!(canonical.system.as_deref(), Some("be terse"));
    // The system message is folded into the prompt, not forwarded.
    assert_eq!(canonical.messages.len(), 1);
    assert_eq!(canonical.messages[0].role, Role::User);
    assert_eq!(canonical.messages[0].text, "hello");
}

#[test]
fn absent_system_prompt_defaults_to_markdown_in_builder() {
    let canonical =
        ChatCompletionsInbound::parse(request(vec![msg("user", "hi")])).expect("valid request");
    // The inbound leaves the system prompt unset...
    assert!(canonical.system.is_none());
    assert_eq!(canonical.messages.len(), 1);

    // ...and the Raycast builder applies the historical `"markdown"` default.
    let raycast = build_chat_request(&canonical, "provider", "model");
    assert_eq!(raycast.system_instructions, "markdown");
}

#[test]
fn forwards_user_and_assistant_in_order() {
    let canonical = ChatCompletionsInbound::parse(request(vec![
        msg("user", "q1"),
        msg("assistant", "a1"),
        msg("user", "q2"),
    ]))
    .expect("valid request");

    let roles: Vec<Role> = canonical.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [Role::User, Role::Assistant, Role::User]);

    let texts: Vec<&str> = canonical.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["q1", "a1", "q2"]);

    // No leading system message → the prompt stays unset.
    assert!(canonical.system.is_none());
}

#[test]
fn drops_unknown_roles() {
    let canonical = ChatCompletionsInbound::parse(request(vec![
        msg("user", "hi"),
        msg("tool", "ignored"),
        msg("function", "ignored too"),
    ]))
    .expect("valid request");

    assert_eq!(canonical.messages.len(), 1);
    assert_eq!(canonical.messages[0].role, Role::User);
}

#[test]
fn non_leading_system_message_is_dropped() {
    // Only a *leading* system message is special. A later one must not become
    // the prompt, and must not be forwarded as a message.
    let canonical =
        ChatCompletionsInbound::parse(request(vec![msg("user", "hi"), msg("system", "too late")]))
            .expect("valid request");

    assert!(canonical.system.is_none());
    assert_eq!(canonical.messages.len(), 1);
    assert_eq!(canonical.messages[0].role, Role::User);
}

#[test]
fn empty_messages_is_rejected() {
    // The inbound guards against an empty conversation.
    assert!(ChatCompletionsInbound::parse(request(vec![])).is_err());
}
