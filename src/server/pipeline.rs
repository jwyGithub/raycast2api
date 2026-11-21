//! The protocol-agnostic request pipeline — component ① orchestration.
//!
//! Every entry handler funnels through [`run`], which owns the flow that is the
//! same for every client protocol: resolve the client-facing model to a Raycast
//! `provider`/`model`, build the upstream request (④), send it, and turn the
//! response into canonical events (⑦). From there the route's chosen
//! [`OutboundAdapter`] (⑤) serializes the result in the caller's protocol —
//! streaming the events directly, or folding them into a [`CanonicalResponse`]
//! for the non-streaming path.
//!
//! `run` is generic over the outbound adapter and names no protocol itself, so
//! adding a protocol is just a new handler that selects its adapters; this glue
//! stays untouched.

use axum::response::Response;
use futures_util::{Stream, StreamExt};

use crate::apikey::{Accounting, Permit};
use crate::core::event::CanonicalEvent;
use crate::core::request::CanonicalRequest;
use crate::core::response::CanonicalResponse;
use crate::error::GatewayError;
use crate::protocol::OutboundAdapter;
use crate::raycast::builder::build_chat_request;
use crate::raycast::wire::RaycastRawModel;
use crate::statistics::{Attempt, CallContext, REQUEST_CONTEXT};

use super::compact::UsageKey;
use super::state::SharedState;

/// Drive one canonical request all the way to a response rendered in the
/// protocol selected by outbound adapter `O`.
pub(crate) async fn run<O: OutboundAdapter>(
    state: &SharedState,
    request: CanonicalRequest,
    model: &RaycastRawModel,
    accounting: Option<Accounting>,
) -> Result<Response, GatewayError> {
    // Resolve remote image references before anything else: the adapters are
    // synchronous (they cannot fetch), and compaction should see the final
    // message shapes.
    let mut request = resolve_remote_attachments(&state.dispatcher.client.http, request).await;
    // Vision gating mirrors the real client: a model without `abilities.vision`
    // gets every image attachment stripped and an omission placeholder in its
    // place — no upload is attempted (B1 in doc/multimodal-images-2026-11-21.md).
    let vision = model_vision_formats(model);
    for message in &mut request.messages {
        if message.attachments.is_empty() {
            continue;
        }
        let kept: Vec<_> = message
            .attachments
            .iter()
            .filter(|a| match a {
                crate::core::message::Attachment::Inline { media_type, .. } => {
                    vision.contains(&media_type.as_str())
                }
                crate::core::message::Attachment::Remote { .. } => !vision.is_empty(),
            })
            .cloned()
            .collect();
        let dropped = message.attachments.len() - kept.len();
        message.attachments = kept;
        if dropped > 0 {
            for _ in 0..dropped {
                message.text.push(' ');
                message
                    .text
                    .push_str(crate::core::message::IMAGE_OMITTED_PLACEHOLDER);
            }
            tracing::info!(dropped, "image attachments dropped: model lacks vision support");
        }
    }
    let events = events(state, &request, model, "inference", accounting, None).await?;
    let client_model = request.model.clone();

    if request.stream {
        Ok(O::stream_with_request(&client_model, &request, events))
    } else {
        match aggregate(events).await {
            Ok(aggregated) => Ok(O::render_complete_with_tools(
                &client_model,
                &request.tools,
                &aggregated,
            )),
            // The upstream failed inside its 200 stream — render the error in
            // the client protocol's own envelope with the real status.
            Err(error) => Ok(O::render_error(error)),
        }
    }
}

pub(crate) async fn admit(
    state: &SharedState,
    requested: &str,
) -> Result<(RaycastRawModel, Option<(Permit, Accounting)>), GatewayError> {
    let model = state.resolve_model(requested).await?;
    let key = REQUEST_CONTEXT
        .try_with(|context| context.managed_key_id.clone())
        .ok()
        .flatten();
    let admission = key
        .map(|id| -> Result<_, GatewayError> {
            let (permit, required) = state.api_keys.admit(&id, &model.id, requested)?;
            Ok((
                permit,
                Accounting {
                    store: state.api_keys.clone(),
                    id,
                    required,
                },
            ))
        })
        .transpose()?;
    Ok((model, admission))
}

pub(crate) fn hold_permit(response: Response, permit: Option<Permit>) -> Response {
    let Some(permit) = permit else {
        return response;
    };
    let (parts, body) = response.into_parts();
    let stream = async_stream::stream! {
        let _permit = permit;
        let mut chunks = body.into_data_stream();
        while let Some(chunk) = chunks.next().await { yield chunk; }
    };
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

/// The selected model's accepted image formats (`abilities.vision.formats` in
/// the raw model extra). Empty when the model has no vision support.
fn model_vision_formats(model: &RaycastRawModel) -> Vec<&'static str> {
    const SUPPORTED: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];
    let Some(formats) = model.extra.get("abilities").and_then(|a| a.get("vision")) else {
        return Vec::new();
    };
    // Two shapes exist in the wild: `vision: {formats: [...]}` (backend
    // catalog) and `vision: {supported: true}` (BYOK mapping). Treat a bare
    // `supported: true` as accepting every format we know.
    if formats.get("supported").and_then(|s| s.as_bool()) == Some(true) {
        return SUPPORTED.to_vec();
    }
    let list = formats
        .get("formats")
        .and_then(|f| f.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|f| f.as_str())
                .filter_map(|f| SUPPORTED.iter().find(|s| s == &&f).copied())
                .collect()
        })
        .unwrap_or_default();
    list
}

/// Fetch every [`Attachment::Remote`] in the request's user messages, replacing
/// each with an inline attachment on success. A failed fetch **drops** the
/// attachment (with a warn) instead of failing the request — the real client
/// degrades missing images the same way. Text is untouched.
async fn resolve_remote_attachments(
    client: &reqwest::Client,
    mut request: CanonicalRequest,
) -> CanonicalRequest {
    let remotes: Vec<(usize, usize, String)> = request
        .messages
        .iter()
        .enumerate()
        .flat_map(|(m, message)| {
            message
                .attachments
                .iter()
                .enumerate()
                .filter_map(move |(a, attachment)| match attachment {
                    crate::core::message::Attachment::Remote { url } => {
                        Some((m, a, url.clone()))
                    }
                    _ => None,
                })
        })
        .collect();
    for (m, a, url) in remotes {
        let fetched = crate::core::attachment::fetch_url(client, &url).await;
        match fetched {
            Some(inline) => {
                request.messages[m].attachments[a] = inline;
            }
            None => {
                request.messages[m].attachments.remove(a);
            }
        }
    }
    request
}

pub(crate) async fn run_admitted<O: OutboundAdapter>(
    state: &SharedState,
    request: CanonicalRequest,
) -> Result<Response, GatewayError> {
    let (model, admission) = admit(state, &request.model).await?;
    let (permit, accounting) = admission.unzip();
    let response = run::<O>(state, request, &model, accounting).await?;
    Ok(hold_permit(response, permit))
}

/// Reserve the first formal attempt before committing SSE headers. The
/// account is selected later, since compaction may use or rotate accounts.
pub(crate) fn reserve_attempt(
    state: &SharedState,
    request: &CanonicalRequest,
    model: &RaycastRawModel,
    accounting: Option<&Accounting>,
) -> Result<Option<Attempt>, GatewayError> {
    let Some(key) = accounting.filter(|key| key.required && !request.compaction_trigger) else {
        return Ok(None);
    };
    let mut call = CallContext::new(request, &model.provider, &model.model, "inference");
    call.accounting = Some(key.clone());
    Attempt::reserve(state.statistics.clone(), &call)
        .map(Some)
        .ok_or(GatewayError::UsageTrackingUnavailable)
}

/// Open the shared upstream path without choosing a client response renderer.
pub(crate) async fn events(
    state: &SharedState,
    request: &CanonicalRequest,
    target: &RaycastRawModel,
    call_type: &'static str,
    accounting: Option<Accounting>,
    reserved: Option<Attempt>,
) -> Result<impl Stream<Item = CanonicalEvent> + Send + 'static, GatewayError> {
    let (provider, model) = (&target.provider, &target.model);
    tracing::info!(
        client_model = %request.model,
        upstream = %format!("{provider}/{model}"),
        stream = request.stream,
        messages = request.messages.len(),
        effort = ?request.reasoning_effort.map(|e| e.as_str()),
        "inference request"
    );
    let raycast_request = build_chat_request(request, provider, model);
    let mut call = CallContext::new(request, provider, model, call_type);
    call.accounting = accounting;
    let usage_key = UsageKey::new(request);

    // 记录发给上游的请求体(与入站请求按 request id 关联)。
    state.log.update_upstream(
        &crate::log::context::current_request_id(),
        &serde_json::to_value(&raycast_request).unwrap_or_default(),
    );
    let response = match state
        .dispatcher
        .send_chat(&raycast_request, &call, reserved)
        .await
    {
        Ok(response) => response,
        Err(error) => {
            if !matches!(error, GatewayError::UsageTrackingUnavailable) {
                state.health.mark_failed();
            }
            return Err(error);
        }
    };

    Ok(track_usage(
        state.clone(),
        usage_key,
        state
            .dispatcher
            .events(response, raycast_request, state.log.clone(), call),
    ))
}

fn track_usage(
    state: SharedState,
    key: UsageKey,
    events: impl Stream<Item = CanonicalEvent> + Send + 'static,
) -> impl Stream<Item = CanonicalEvent> + Send + 'static {
    events.then(move |event| {
        let state = state.clone();
        async move {
            match &event {
                CanonicalEvent::Finish { usage, .. } => {
                    state.health.mark_working();
                    state.compact.record_usage(key, usage.input_tokens).await;
                }
                CanonicalEvent::Error { .. } => state.health.mark_failed(),
                _ => {}
            }
            event
        }
    })
}

/// Consume the whole canonical event stream, folding it into a single
/// [`CanonicalResponse`] for the non-streaming path. An in-stream upstream
/// error aborts the fold with a [`GatewayError::Upstream`].
async fn aggregate(
    events: impl Stream<Item = CanonicalEvent>,
) -> Result<CanonicalResponse, GatewayError> {
    let mut events = std::pin::pin!(events);
    let mut response = CanonicalResponse::default();

    while let Some(event) = events.next().await {
        match event {
            CanonicalEvent::TextDelta(text) => response.text.push_str(&text),
            CanonicalEvent::ReasoningDelta(text) => response.reasoning.push_str(&text),
            CanonicalEvent::ThoughtSignature(signature) => {
                response.thought_signature = Some(signature)
            }
            CanonicalEvent::ToolCall(call) => response.tool_calls.push(call),
            // The generated image itself arrives as a markdown TextDelta; the
            // start marker carries nothing to aggregate.
            CanonicalEvent::ImageGenerationStarted { .. } => {}
            CanonicalEvent::Finish { reason, usage } => {
                response.finish_reason = reason;
                response.usage = usage;
            }
            CanonicalEvent::Error { status, message } => {
                return Err(GatewayError::Upstream { status, message });
            }
        }
    }

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        message::{Message, Role},
        protocol::Protocol,
        response::FinishReason,
        usage::Usage,
    };

    #[tokio::test]
    async fn finish_records_input_usage_and_preserves_client_usage_for_both_paths() {
        for streaming in [false, true] {
            let state = super::super::state::compact_test_state();
            let mut request = CanonicalRequest::new(Protocol::Responses, "test-model");
            request.stream = streaming;
            request.messages = vec![Message::text(Role::User, "seed")];
            let usage = Usage {
                input_tokens: 46_690,
                output_tokens: 1411,
                cached_tokens: 42_122,
            };
            let source = vec![
                CanonicalEvent::TextDelta("done".into()),
                CanonicalEvent::Finish {
                    reason: FinishReason::Stop,
                    usage,
                },
            ];
            let events = track_usage(
                state.clone(),
                UsageKey::new(&request),
                futures_util::stream::iter(source.clone()),
            );
            if streaming {
                assert_eq!(events.collect::<Vec<_>>().await, source);
            } else {
                let response = aggregate(events).await.unwrap();
                assert_eq!(response.text, "done");
                assert_eq!(response.usage, usage);
            }
            request.messages.extend([
                Message::text(Role::Assistant, "done"),
                Message::text(Role::User, "next"),
            ]);
            assert_eq!(state.compact.estimate_tokens(&request).await, Some(46_692));
        }
    }

    #[tokio::test]
    async fn errors_and_missing_usage_do_not_create_measurements() {
        for terminal in [
            None,
            Some(CanonicalEvent::Error {
                status: 502,
                message: "failed".into(),
            }),
            Some(CanonicalEvent::Finish {
                reason: FinishReason::Stop,
                usage: Usage::default(),
            }),
        ] {
            let state = super::super::state::compact_test_state();
            let mut request = CanonicalRequest::new(Protocol::Responses, "test-model");
            request.messages = vec![Message::text(Role::User, "seed")];
            let events = track_usage(
                state.clone(),
                UsageKey::new(&request),
                futures_util::stream::iter(terminal),
            );
            let _: Vec<_> = events.collect().await;
            assert_eq!(state.compact.estimate_tokens(&request).await, None);
        }
    }
}
