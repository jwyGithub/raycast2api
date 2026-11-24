//! Usage accounting at the upstream boundary, independent of diagnostic logs.
pub mod store;
#[cfg(test)]
#[path = "../../tests/unit/statistics.rs"]
mod tests;
pub mod wire;

use serde_json::Value;
use std::sync::{Arc, Mutex};

use crate::{
    core::request::CanonicalRequest,
    key::{Accounting, KeyId, wire::usage_units},
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
    pub managed_key_id: Option<KeyId>,
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
    pub accounting: Option<Accounting>,
}

impl CallContext {
    pub fn new(
        request: &CanonicalRequest,
        provider: &str,
        model: &str,
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
                    managed_key_id: None,
                    request_ip: None,
                }),
            requested_model: request.model.clone(),
            requested_effort: request.requested_effort.clone(),
            requested_thinking_json: request.requested_thinking.as_ref().map(Value::to_string),
            upstream_provider: provider.into(),
            upstream_model: model.into(),
            upstream_effort: request
                .reasoning_effort
                .map(|effort| effort.as_str().into()),
            call_type,
            accounting: None,
        }
    }
}

/// Owned by the response, then its decoder. Dropping either also records cancellation.
#[derive(Clone)]
pub(crate) struct Attempt(Arc<AttemptInner>);

struct AttemptInner {
    store: Arc<StatisticsStore>,
    id: String,
    accounting: Option<Accounting>,
    state: Mutex<AttemptState>,
}

#[derive(Default)]
struct AttemptState {
    finished: bool,
    reserved: bool,
    http_status: Option<i32>,
    usage: Option<Value>,
    terminal: Option<Terminal>,
}

struct Terminal {
    status: String,
    error: Option<String>,
    usage: Option<Value>,
    http_status: Option<i32>,
    amounts: Option<(i64, i64)>,
}

impl Attempt {
    pub fn start(store: Arc<StatisticsStore>, call: &CallContext, email: &str) -> Option<Self> {
        Self::insert(store, call, Some(email))
    }

    pub fn reserve(store: Arc<StatisticsStore>, call: &CallContext) -> Option<Self> {
        Self::insert(store, call, None)
    }

    fn insert(
        store: Arc<StatisticsStore>,
        call: &CallContext,
        email: Option<&str>,
    ) -> Option<Self> {
        let id = new_ulid();
        if let Err(error) = store.start(&id, call, email) {
            tracing::error!(%error, managed_key_id = call.request.managed_key_id.as_ref().map(|id| id.0.as_str()), "statistics insert failed");
            return None;
        }
        Some(Self(Arc::new(AttemptInner {
            store,
            id,
            accounting: call
                .accounting
                .clone()
                .filter(|_| call.call_type == "inference"),
            state: Mutex::new(AttemptState {
                reserved: email.is_none(),
                ..Default::default()
            }),
        })))
    }

    pub fn bind_account(&self, email: &str) -> Result<(), crate::error::GatewayError> {
        self.0.store.bind_account(&self.0.id, email).map_err(|error| {
            tracing::error!(%error, id = %self.0.id, "failed to activate reserved statistics record");
            crate::error::GatewayError::UsageTrackingUnavailable
        })?;
        self.0.state.lock().unwrap().reserved = false;
        Ok(())
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
        if state.terminal.is_none() {
            let usage = state.usage.take();
            let amounts = (status == "completed" && self.accounting.is_some()).then(|| {
                let amount = |field| {
                    usage.as_ref().and_then(|usage| usage.get(field)).and_then(usage_units).unwrap_or_else(|| {
                        tracing::warn!(id = %self.id, field, "missing or invalid managed API key usage");
                        0
                    })
                };
                (amount("total_credits"), amount("total_cost"))
            });
            state.terminal = Some(Terminal {
                status: status.into(),
                error: error.map(str::to_owned),
                usage,
                http_status: state.http_status,
                amounts,
            });
        }
        let terminal = state.terminal.as_ref().unwrap();
        let settlement = self.accounting.as_ref().zip(terminal.amounts);
        match self.store.finish(&self.id, terminal, settlement) {
            Ok(_) => state.finished = true,
            Err(error) => tracing::error!(%error, id = %self.id, "statistics finish failed"),
        }
    }
}

impl Drop for AttemptInner {
    fn drop(&mut self) {
        if self.state.get_mut().unwrap().reserved {
            if let Err(error) = self.store.discard_reservation(&self.id) {
                tracing::error!(%error, id = %self.id, "could not discard unused statistics reservation");
            }
            return;
        }
        self.finish(
            "cancelled",
            Some("upstream attempt dropped before a terminal event"),
        );
    }
}
