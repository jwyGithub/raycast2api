//! Usage accounting at the upstream boundary, independent of diagnostic logs.
pub mod store;
#[cfg(test)]
mod tests;
pub mod wire;

use serde_json::Value;
use std::sync::{Arc, Mutex};

use crate::{
    core::request::CanonicalRequest,
    raycast::wire::RaycastChatRequest,
    util::{new_ulid, unix_secs},
};
use store::StatisticsStore;

tokio::task_local! {
    pub(crate) static REQUEST_CONTEXT: RequestContext;
}

#[derive(Clone)]
pub(crate) struct RequestContext {
    pub request_id: String,
    pub requested_at: i64,
    pub api_key: Option<String>,
    pub api_key_masked: Option<String>,
    pub request_ip: Option<String>,
}

#[derive(Clone)]
pub(crate) struct CallContext {
    pub request: RequestContext,
    pub requested_model: String,
    pub requested_effort: Option<String>,
    pub requested_thinking_json: Option<String>,
    pub upstream_provider: String,
    pub upstream_model: String,
    pub upstream_effort: Option<String>,
    pub call_type: &'static str,
}

impl CallContext {
    pub fn new(
        request: &CanonicalRequest,
        upstream: &RaycastChatRequest,
        call_type: &'static str,
    ) -> Self {
        Self {
            request: REQUEST_CONTEXT
                .try_with(Clone::clone)
                .unwrap_or_else(|_| RequestContext {
                    request_id: crate::log::context::current_request_id(),
                    requested_at: unix_secs() as i64,
                    api_key: None,
                    api_key_masked: None,
                    request_ip: None,
                }),
            requested_model: request.model.clone(),
            requested_effort: request.requested_effort.clone(),
            requested_thinking_json: request.requested_thinking.as_ref().map(Value::to_string),
            upstream_provider: upstream.provider.clone(),
            upstream_model: upstream.model.clone(),
            upstream_effort: upstream.reasoning_effort.clone(),
            call_type,
        }
    }
}

/// Owned by the response, then its decoder. Dropping either also records cancellation.
#[derive(Clone)]
pub(crate) struct Attempt(Arc<AttemptInner>);

struct AttemptInner {
    store: Arc<StatisticsStore>,
    id: String,
    state: Mutex<AttemptState>,
}

#[derive(Default)]
struct AttemptState {
    finished: bool,
    http_status: Option<i32>,
    usage: Option<Value>,
}

impl Attempt {
    pub fn start(store: Arc<StatisticsStore>, call: &CallContext, email: &str) -> Option<Self> {
        let id = new_ulid();
        if let Err(error) = store.start(&id, call, email) {
            tracing::error!(%error, "statistics insert failed");
            return None;
        }
        Some(Self(Arc::new(AttemptInner {
            store,
            id,
            state: Mutex::default(),
        })))
    }

    pub fn set_http_status(&self, status: u16) {
        self.0.state.lock().unwrap().http_status = Some(i32::from(status));
    }

    pub fn observe_payload(&self, payload: &str) {
        if let Ok(value) = serde_json::from_str::<Value>(payload)
            && let Some(usage) = value.get("usage").filter(|usage| usage.is_object())
        {
            self.0.state.lock().unwrap().usage = Some(usage.clone());
        }
    }

    pub fn finish(&self, status: &str, error: Option<&str>) {
        self.0.finish(status, error);
    }

    pub fn fail(&self, error: &crate::error::GatewayError) {
        self.finish("failed", Some(&error.to_string()));
    }
}

impl AttemptInner {
    fn finish(&self, status: &str, error: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        if state.finished {
            return;
        }
        match self.store.finish(
            &self.id,
            status,
            state.http_status,
            error,
            state.usage.as_ref(),
        ) {
            Ok(()) => state.finished = true,
            Err(error) => tracing::error!(%error, id = %self.id, "statistics finish failed"),
        }
    }
}

impl Drop for AttemptInner {
    fn drop(&mut self) {
        self.finish(
            "cancelled",
            Some("upstream attempt dropped before a terminal event"),
        );
    }
}
