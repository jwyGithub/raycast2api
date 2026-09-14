//! ② OpenAI Chat Completions inbound: [`ChatCompletionRequest`] →
//! [`CanonicalRequest`].
//!
//! The mapping preserves the gateway's original `convert_messages` rules
//! exactly: a *leading* `system` message becomes the canonical `system`
//! (otherwise it is left unset, and the Raycast builder defaults it to
//! `"markdown"`); `user`/`assistant` messages are forwarded verbatim and in
//! order; every other role is dropped. `temperature` is carried through as-is
//! (the builder applies the `0.5` default), and a missing `model` falls back to
//! [`DEFAULT_MODEL_ID`].

use crate::config::DEFAULT_MODEL_ID;
use crate::core::message::{Message, Role};
use crate::core::protocol::Protocol;
use crate::core::request::CanonicalRequest;
use crate::error::GatewayError;
use crate::protocol::InboundAdapter;

use super::wire::{ChatCompletionRequest, ChatMessage};

/// Zero-sized marker selecting the OpenAI Chat Completions inbound mapping.
pub struct ChatCompletionsInbound;

impl InboundAdapter for ChatCompletionsInbound {
    type Request = ChatCompletionRequest;

    fn parse(request: ChatCompletionRequest) -> Result<CanonicalRequest, GatewayError> {
        if request.messages.is_empty() {
            return Err(GatewayError::BadRequest(
                "Missing or invalid 'messages' field".into(),
            ));
        }

        let model = request
            .model
            .unwrap_or_else(|| DEFAULT_MODEL_ID.to_string());
        let mut canonical = CanonicalRequest::new(Protocol::ChatCompletions, model);
        canonical.stream = request.stream;
        canonical.temperature = request.temperature;
        canonical.system = system_instructions(&request.messages);
        canonical.messages = conversation(&request.messages);

        Ok(canonical)
    }
}

/// A *leading* `system` message becomes the system prompt; anything else leaves
/// it unset (the builder then falls back to its default).
fn system_instructions(messages: &[ChatMessage]) -> Option<String> {
    match messages.first() {
        Some(first) if first.role == "system" => Some(first.content.clone()),
        _ => None,
    }
}

/// Forward `user`/`assistant` messages in order; drop `system` and any other
/// role (a leading `system` is already folded into the system prompt).
fn conversation(messages: &[ChatMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|message| match message.role.as_str() {
            "user" => Some(Message::text(Role::User, message.content.clone())),
            "assistant" => Some(Message::text(Role::Assistant, message.content.clone())),
            _ => None,
        })
        .collect()
}
