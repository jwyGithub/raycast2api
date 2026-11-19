//! `POST /v1/chat/completions` — OpenAI-compatible chat, streaming or not.
//!
//! The handler is deliberately thin: it names the OpenAI adapters — inbound (②)
//! and outbound (⑤) — and hands off to the shared
//! [`pipeline`](crate::server::pipeline), which owns the protocol-agnostic flow
//! (resolve the model, build the upstream request, send it, and turn the
//! response into canonical events). All OpenAI-specific mapping lives in the
//! adapters; all provider-specific work lives in the Raycast layer.

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::error::GatewayError;
use crate::protocol::chat_completions::wire::ChatCompletionRequest;
use crate::protocol::chat_completions::{ChatCompletionsInbound, ChatCompletionsOutbound};
use crate::protocol::{InboundAdapter, OutboundAdapter};
use crate::server::pipeline;
use crate::server::state::SharedState;

pub async fn chat_completions(
    State(state): State<SharedState>,
    request: Result<Json<ChatCompletionRequest>, JsonRejection>,
) -> Response {
    let request = match request {
        Ok(Json(request)) => request,
        Err(error) => {
            let status = if error.status() == StatusCode::UNPROCESSABLE_ENTITY {
                StatusCode::BAD_REQUEST
            } else {
                error.status()
            };
            let mut response = GatewayError::BadRequest(error.body_text()).into_response();
            *response.status_mut() = status;
            return response;
        }
    };
    let result = match ChatCompletionsInbound::parse(request) {
        Ok(canonical) => pipeline::run_admitted::<ChatCompletionsOutbound>(&state, canonical).await,
        Err(error) => Err(error),
    };
    result.unwrap_or_else(ChatCompletionsOutbound::render_error)
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn tools_round_trip_through_router_pipeline_and_local_upstream() {
        use axum::{Json, Router, routing::post};
        use serde_json::{Value, json};
        use std::sync::Arc;

        for streaming in [false, true] {
            let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
            let upstream = Router::new().route("/", post(move |Json(body): Json<Value>| {
                let sent = sent.clone();
                async move {
                    let has_result = body["messages"].as_array().unwrap().iter()
                        .any(|m| m["role"] == "tool");
                    sent.send(body).unwrap();
                    let payload = if has_result {
                        json!({"text":"Sunny","finish_reason":"stop","usage":{"input_tokens":10,"output_tokens":2}})
                    } else {
                        json!({"text":"Checking","tool_calls":[{"id":"call_1","name":"weather","arguments":"{ \"city\": \"SF\" }"}],"finish_reason":"tool_calls","usage":{"input_tokens":5,"output_tokens":3}})
                    };
                    format!("data: {payload}\n\n")
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, upstream).await.unwrap();
            });
            let mut state = crate::server::state::compact_test_state();
            let inner = Arc::get_mut(&mut state).unwrap();
            inner.dispatcher.client.http = reqwest::Client::builder().no_proxy().build().unwrap();
            inner.dispatcher.client.chat_url = Some(format!("http://{address}/"));
            inner
                .accounts
                .upsert(
                    &serde_json::from_value(
                        json!({"user":{"email":"test@example.com"},"access_token":"fixture-token"}),
                    )
                    .unwrap(),
                )
                .unwrap();
            let app = crate::server::build_router(state);
            let mut body = json!({"model":"test-model", "stream":streaming, "temperature":0.7,"reasoning_effort":"high",
                "tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object"}}}],
                "tool_choice":"auto",
                "messages":[{"role":"system","content":"Be brief"},{"role":"user","content":[{"type":"text","text":"weather?"}]}]
            });
            if streaming {
                body["stream_options"] = json!({"include_usage":true});
            }
            for turn in 0..2 {
                let request = Request::post("/v1/chat/completions")
                    .header("x-api-key", "test-key")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap();
                let response = app.clone().oneshot(request).await.unwrap();
                assert_eq!(response.status(), axum::http::StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let mut assistant;
                if streaming {
                    let text = String::from_utf8(bytes.to_vec()).unwrap();
                    assert!(text.ends_with("data: [DONE]\n\n"));
                    let chunks: Vec<Value> = text
                        .lines()
                        .filter_map(|l| l.strip_prefix("data: "))
                        .filter(|l| *l != "[DONE]")
                        .map(|l| serde_json::from_str(l).unwrap())
                        .collect();
                    assert_eq!(chunks.last().unwrap()["choices"], json!([]));
                    assert_eq!(
                        chunks.last().unwrap()["usage"]["prompt_tokens"],
                        if turn == 0 { 5 } else { 10 }
                    );
                    let finish = &chunks[chunks.len() - 2]["choices"][0]["finish_reason"];
                    assert_eq!(finish, if turn == 0 { "tool_calls" } else { "stop" });
                    assistant = json!({"role":"assistant","content":""});
                    let mut calls = Vec::new();
                    for chunk in &chunks[..chunks.len() - 1] {
                        let delta = &chunk["choices"][0]["delta"];
                        if let Some(text) = delta["content"].as_str() {
                            assistant["content"] = json!(text);
                        }
                        if let Some(tools) = delta["tool_calls"].as_array() {
                            for tool in tools {
                                let mut tool = tool.clone();
                                tool.as_object_mut().unwrap().remove("index");
                                calls.push(tool);
                            }
                        }
                    }
                    if !calls.is_empty() {
                        assistant["tool_calls"] = json!(calls);
                    }
                } else {
                    let value: Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(
                        value["choices"][0]["finish_reason"],
                        if turn == 0 { "tool_calls" } else { "stop" }
                    );
                    assert_eq!(
                        value["usage"]["prompt_tokens"],
                        if turn == 0 { 5 } else { 10 }
                    );
                    assistant = value["choices"][0]["message"].clone();
                }
                let captured = received.try_recv().unwrap();
                assert_eq!(captured["model"], "test");
                assert_eq!(captured["additional_system_instructions"], "Be brief");
                assert_eq!(captured["reasoning_effort"], "high");
                assert_eq!(captured["temperature"], 0.7);
                assert_eq!(captured["tools"][0]["type"], "local_tool");
                if turn == 0 {
                    assert_eq!(assistant["content"], "Checking");
                    assert_eq!(
                        assistant["tool_calls"][0]["function"]["arguments"],
                        "{ \"city\": \"SF\" }"
                    );
                    body["messages"].as_array_mut().unwrap().extend([
                        assistant,
                        json!({"role":"tool","tool_call_id":"call_1","content":[{"type":"text","text":"sunny"}]})
                    ]);
                } else {
                    assert_eq!(assistant["content"], "Sunny");
                    assert_eq!(captured["messages"][1]["tool_calls"][0]["id"], "call_1");
                    assert_eq!(captured["messages"][2]["tool_call_id"], "call_1");
                    assert_eq!(captured["messages"][2]["name"], "weather");
                    assert_eq!(captured["messages"][2]["content"]["text"], "sunny");
                }
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn request_errors_use_json_and_preserve_body_limit_status() {
        let app = crate::server::build_router(crate::server::state::compact_test_state());
        for (body, content_type, expected) in [
            ("{".to_owned(), Some("application/json"), 400),
            (
                r#"{"model":"test-model","messages":[{"role":"user","content":42}]}"#.to_owned(),
                Some("application/json"),
                400,
            ),
            (
                r#"{"messages":[{"role":"user","content":"hi"}]}"#.to_owned(),
                Some("application/json"),
                400,
            ),
            (
                r#"{"model":"unknown","messages":[{"role":"user","content":"hi"}]}"#.to_owned(),
                Some("application/json"),
                404,
            ),
            ("{}".to_owned(), None, 415),
            (
                " ".repeat(16 * 1024 * 1024 + 1),
                Some("application/json"),
                413,
            ),
        ] {
            let mut request = Request::post("/v1/chat/completions").header("x-api-key", "test-key");
            if let Some(content_type) = content_type {
                request = request.header("content-type", content_type);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::from(body)).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), expected);
            assert_eq!(response.headers()["content-type"], "application/json");
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"]["type"], "invalid_request_error");
            assert!(body["error"]["message"].is_string());
        }
    }
}
