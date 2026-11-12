//! `POST /v1/responses` — OpenAI Responses, streaming or not.
//!
//! Streaming responses open before compaction and report progress through a
//! complete reasoning item, then continue inference in the same SSE response.
//! Non-streaming responses compact before entering the shared pipeline.

use async_stream::stream;
use axum::Json;
use axum::extract::State;
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::{StreamExt, future::BoxFuture, stream::BoxStream};

use crate::core::event::CanonicalEvent;
use crate::core::request::CanonicalRequest;
use crate::debug::{self, REQUEST_ID};
use crate::error::GatewayError;
use crate::protocol::InboundAdapter;
use crate::protocol::responses::outbound::{StreamEncoder, custom_names, to_event};
use crate::protocol::responses::wire::ResponsesRequest;
use crate::protocol::responses::{ResponsesInbound, ResponsesOutbound};
use crate::raycast::builder::build_chat_request;
use crate::raycast::stream::canonical_events_with_retry;
use crate::server::compact;
use crate::server::pipeline;
use crate::server::state::SharedState;
use crate::util::{new_uuid, unix_secs};

pub async fn responses(
    State(state): State<SharedState>,
    Json(request): Json<ResponsesRequest>,
) -> Result<Response, GatewayError> {
    let canonical = ResponsesInbound::parse(request)?;
    if canonical.stream {
        return Ok(stream_response(state, canonical));
    }
    // Auto compact: when the conversation crosses ~90% of the model context
    // window, summarize the prefix and rewrite the history first. The closure
    // runs the summarization completion through the upstream and folds it to
    // plain text (no recursion — `maybe_compact` is called only from here).
    let canonical = compact::maybe_compact(&state, canonical, |summary_request| {
        let state = state.clone();
        Box::pin(async move { summarize(&state, summary_request).await })
    })
    .await?;
    pipeline::run::<ResponsesOutbound>(&state, canonical).await
}

/// Keep compaction and inference in one response, with no detached background task.
fn stream_response(state: SharedState, request: CanonicalRequest) -> Response {
    let summary_state = state.clone();
    let inference_state = state.clone();
    stream_response_with(
        state,
        request,
        move |request| Box::pin(async move { summarize(&summary_state, request).await }),
        move |request| {
            Box::pin(async move { Ok(pipeline::events(&inference_state, &request).await?.boxed()) })
        },
    )
}

fn stream_response_with(
    state: SharedState,
    request: CanonicalRequest,
    summarize: impl FnOnce(CanonicalRequest) -> BoxFuture<'static, Result<String, GatewayError>>
    + Send
    + 'static,
    infer: impl FnOnce(
        CanonicalRequest,
    )
        -> BoxFuture<'static, Result<BoxStream<'static, CanonicalEvent>, GatewayError>>
    + Send
    + 'static,
) -> Response {
    let mut encoder = StreamEncoder::new(
        format!("resp_{}", new_uuid()),
        request.model.clone(),
        unix_secs(),
    );
    encoder.set_custom(custom_names(&request.tools));
    let channel = RequestChannel(debug::current_request_id());
    let frames = stream! {
        yield to_event(&encoder.created());
        let plan = compact::plan(&state, &request).await;
        let compacting = !matches!(plan, compact::Compaction::NotNeeded);
        if compacting {
            for frame in encoder.push(CanonicalEvent::ReasoningDelta("正在压缩上下文…\n".into())) {
                yield to_event(&frame);
            }
        }
        let result = compact::apply(&state, request, plan, summarize).await;
        match result {
            Ok(request) => {
                if compacting {
                    for frame in encoder.push(CanonicalEvent::ReasoningDelta("上下文压缩完成，继续处理任务。\n".into())) {
                        yield to_event(&frame);
                    }
                    for frame in encoder.close_current() {
                        yield to_event(&frame);
                    }
                }
                match infer(request).await {
                    Ok(events) => {
                        let mut events = std::pin::pin!(events);
                        while let Some(event) = events.next().await {
                            let terminal = matches!(event, CanonicalEvent::Finish { .. } | CanonicalEvent::Error { .. });
                            for frame in encoder.push(event) {
                                yield to_event(&frame);
                            }
                            if terminal { break; }
                        }
                    }
                    Err(error) => { encoder.push(stream_error(error)); }
                }
            }
            Err(error) => { encoder.push(stream_error(error)); }
        }
        for frame in encoder.finalize() {
            yield to_event(&frame);
        }
    };
    let scoped = stream! {
        let mut frames = std::pin::pin!(frames);
        // Body polling happens after the middleware's task-local scope has ended.
        while let Some(frame) = REQUEST_ID.scope(channel.0.clone(), frames.next()).await {
            yield frame;
        }
    };
    Sse::new(scoped)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn stream_error(error: GatewayError) -> CanonicalEvent {
    let (status, message) = error.status_and_message();
    CanonicalEvent::Error {
        status: status.as_u16(),
        message,
    }
}

// The middleware returns before deferred upstream calls register their channel.
struct RequestChannel(String);

impl Drop for RequestChannel {
    fn drop(&mut self) {
        debug::take_channel(&self.0);
    }
}

/// Run one plain completion through the upstream and return its text — used
/// for the compact summarization call.
async fn summarize(
    state: &SharedState,
    request: crate::core::request::CanonicalRequest,
) -> Result<String, GatewayError> {
    let (provider, model) = state.resolve_model(&request.model).await;
    let raycast_request = build_chat_request(&request, &provider, &model);
    let response = state.client.send_chat(&raycast_request).await?;
    let events = canonical_events_with_retry(
        response,
        state.client.clone(),
        raycast_request,
        state.log.clone(),
    );
    let mut events = std::pin::pin!(events);
    let mut text = String::new();
    while let Some(event) = events.next().await {
        match event {
            CanonicalEvent::TextDelta(delta) => text.push_str(&delta),
            CanonicalEvent::Finish { .. } => break,
            CanonicalEvent::Error { status, message } => {
                return Err(GatewayError::Upstream { status, message });
            }
            _ => {}
        }
    }
    Ok(text)
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
    use serde_json::{Value, json};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn request() -> CanonicalRequest {
        let mut request = CanonicalRequest::new(Protocol::Responses, "test-model");
        request.stream = true;
        request.messages = vec![
            Message::text(Role::Assistant, "x".repeat(90_000)),
            Message::text(Role::User, "CURRENT_TASK"),
        ];
        request
    }

    fn decode(bytes: &[u8]) -> Vec<Value> {
        std::str::from_utf8(bytes)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn progress_arrives_before_summary_then_inference_in_same_response() {
        let state = crate::server::state::compact_test_state();
        let stage = Arc::new(AtomicUsize::new(0));
        let summary_stage = stage.clone();
        let inference_stage = stage.clone();
        let response = REQUEST_ID
            .scope("compact-stream-test".into(), async {
                stream_response_with(
                    state,
                    request(),
                    move |_| {
                        Box::pin(async move {
                            assert_eq!(debug::current_request_id(), "compact-stream-test");
                            assert_eq!(summary_stage.fetch_add(1, Ordering::SeqCst), 0);
                            Ok("summary".into())
                        })
                    },
                    move |request| {
                        Box::pin(async move {
                            assert_eq!(debug::current_request_id(), "compact-stream-test");
                            assert_eq!(request.messages.last().unwrap().text, "CURRENT_TASK");
                            assert_eq!(inference_stage.fetch_add(1, Ordering::SeqCst), 1);
                            Ok(futures_util::stream::iter(vec![
                                CanonicalEvent::ReasoningDelta("MODEL_REASONING".into()),
                                CanonicalEvent::TextDelta("NEW_ANSWER".into()),
                                CanonicalEvent::Finish {
                                    reason: FinishReason::Stop,
                                    usage: Usage::default(),
                                },
                            ])
                            .boxed())
                        })
                    },
                )
            })
            .await;
        let mut body = response.into_body().into_data_stream();
        let mut bytes = Vec::new();
        loop {
            let chunk = body.next().await.unwrap().unwrap();
            bytes.extend_from_slice(&chunk);
            if decode(&bytes)
                .iter()
                .any(|v| v["type"] == "response.reasoning_summary_text.delta")
            {
                break;
            }
        }
        assert_eq!(
            stage.load(Ordering::SeqCst),
            0,
            "progress must be delivered before waiting for summary"
        );
        while let Some(chunk) = body.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(stage.load(Ordering::SeqCst), 2);
        let frames = decode(&bytes);
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame["sequence_number"], i);
        }
        assert_eq!(
            frames
                .iter()
                .filter(|f| f["type"] == "response.created")
                .count(),
            1
        );
        assert_eq!(
            frames
                .iter()
                .filter(|f| f["type"] == "response.completed")
                .count(),
            1
        );
        let created = &frames[0]["response"]["id"];
        let completed = frames.last().unwrap();
        assert_eq!(&completed["response"]["id"], created);
        let output = completed["response"]["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "reasoning");
        assert!(
            output[0]["summary"][0]["text"]
                .as_str()
                .unwrap()
                .contains("上下文压缩完成")
        );
        assert_eq!(output[1]["summary"][0]["text"], "MODEL_REASONING");
        assert_eq!(output[2]["content"][0]["text"], "NEW_ANSWER");
        let mut active = None;
        for frame in &frames {
            match frame["type"].as_str().unwrap() {
                "response.output_item.added" => {
                    assert!(active.is_none());
                    active = Some(frame["item"]["id"].clone());
                }
                "response.reasoning_summary_text.delta" => {
                    assert_eq!(active.as_ref(), Some(&frame["item_id"]))
                }
                "response.output_item.done" => {
                    assert_eq!(active.take(), Some(frame["item"]["id"].clone()))
                }
                _ => {}
            }
        }
        assert!(active.is_none());
    }

    #[tokio::test]
    async fn summary_failure_emits_failed_without_inference_or_completed() {
        let state = crate::server::state::compact_test_state();
        let response = stream_response_with(
            state,
            request(),
            |_| {
                Box::pin(async {
                    Err(GatewayError::Upstream {
                        status: 503,
                        message: "summary failed".into(),
                    })
                })
            },
            |_| panic!("inference must not run after failed compaction"),
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let frames = decode(&bytes);
        assert_eq!(frames.last().unwrap()["type"], "response.failed");
        assert!(!frames.iter().any(|v| v["type"] == "response.completed"));
        assert!(
            frames.last().unwrap()["response"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("summary failed")
        );
    }

    #[tokio::test]
    async fn dropping_progress_stream_cancels_compaction() {
        let state = crate::server::state::compact_test_state();
        let response = stream_response_with(
            state,
            request(),
            |_| panic!("summary must not start after cancellation"),
            |_| panic!("inference must not start"),
        );
        let mut body = response.into_body().into_data_stream();
        let mut bytes = Vec::new();
        loop {
            bytes.extend_from_slice(&body.next().await.unwrap().unwrap());
            if decode(&bytes)
                .iter()
                .any(|v| v["type"] == "response.reasoning_summary_text.delta")
            {
                break;
            }
        }
        drop(body);
    }

    #[tokio::test]
    async fn short_request_skips_progress_and_keeps_custom_tools() {
        let state = crate::server::state::compact_test_state();
        let mut request = request();
        request.messages.remove(0);
        request.tools.push(crate::core::tools::ToolDef::Custom {
            name: "apply_patch".into(),
            description: None,
            grammar: String::new(),
        });
        let response = stream_response_with(
            state,
            request,
            |_| panic!("no compaction needed"),
            |_| {
                Box::pin(async {
                    Ok(futures_util::stream::iter(vec![
                        CanonicalEvent::ToolCall(crate::core::message::ToolCall {
                            id: "call_1".into(),
                            name: "apply_patch".into(),
                            arguments: json!({"input": "PATCH"}).to_string(),
                        }),
                        CanonicalEvent::Finish {
                            reason: FinishReason::ToolCalls,
                            usage: Usage::default(),
                        },
                    ])
                    .boxed())
                })
            },
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let frames = decode(&bytes);
        let output = &frames.last().unwrap()["response"]["output"];
        assert_eq!(output.as_array().unwrap().len(), 1);
        assert_eq!(output[0]["type"], "custom_tool_call");
        assert_eq!(output[0]["input"], "PATCH");
    }
    #[tokio::test]
    async fn disconnect_drops_in_flight_summary_and_cleans_request_channel() {
        struct Cancelled(Arc<AtomicUsize>);
        impl Drop for Cancelled {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let state = crate::server::state::compact_test_state();
        let cancelled = Arc::new(AtomicUsize::new(0));
        let summary_cancelled = cancelled.clone();
        let response = REQUEST_ID
            .scope("compact-cancel-test".into(), async {
                stream_response_with(
                    state,
                    request(),
                    move |_| {
                        Box::pin(async move {
                            let _guard = Cancelled(summary_cancelled);
                            debug::record_channel(&debug::current_request_id(), "test-proxy");
                            std::future::pending().await
                        })
                    },
                    |_| panic!("inference must not start"),
                )
            })
            .await;
        let mut body = response.into_body().into_data_stream();
        // Poll through all progress frames until the summary is actually pending.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), async {
                while let Some(chunk) = body.next().await {
                    chunk.unwrap();
                }
            })
            .await
            .is_err()
        );
        assert_eq!(cancelled.load(Ordering::SeqCst), 0);
        drop(body);
        assert_eq!(cancelled.load(Ordering::SeqCst), 1);
        assert!(debug::take_channel("compact-cancel-test").is_none());
    }

    #[tokio::test]
    async fn inference_connection_failure_after_compaction_emits_failed() {
        let state = crate::server::state::compact_test_state();
        let response = stream_response_with(
            state,
            request(),
            |_| Box::pin(async { Ok("summary".into()) }),
            |_| Box::pin(async { Err(GatewayError::NoAccount("no account".into())) }),
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let frames = decode(&bytes);
        assert_eq!(frames.last().unwrap()["type"], "response.failed");
        assert!(!frames.iter().any(|f| f["type"] == "response.completed"));
        assert!(frames.iter().any(|f| {
            f["type"] == "response.output_item.done"
                && f["item"]["summary"][0]["text"]
                    .as_str()
                    .is_some_and(|s| s.contains("上下文压缩完成"))
        }));
    }
}
