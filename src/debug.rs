//! In-process debug event bus for the local development console.
//!
//! A lightweight, opt-in observability surface used only by the `/debug`
//! console: the inbound request captured by the gateway's `debug_capture`
//! middleware and the upstream request issued by the Raycast client are both
//! published to a [`tokio::sync::broadcast`] channel, which the SSE endpoint
//! (`GET /debug/events`) fans out to connected browsers in real time.
//!
//! It is deliberately protocol- and layer-neutral (no `axum`, no `reqwest`) so
//! both the server and the Raycast client can depend on it without a circular
//! dependency. Callers render their own header maps into `[name, value]` pairs
//! (the two live in different crates' `http` re-exports) and hand over plain
//! data. Events carry a correlation id threaded through a task-local, so no
//! handler or pipeline signature has to know the console exists.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

/// How many recent events the channel retains for a briefly-lagging subscriber.
const CHANNEL_CAPACITY: usize = 256;

tokio::task_local! {
    /// Correlation id for the request currently being handled. Set by the
    /// `debug_capture` middleware so the Raycast client can tag its `Upstream`
    /// event to match the `Inbound` one — without threading an id through every
    /// handler and pipeline signature.
    pub static REQUEST_ID: String;
}

/// The current request's correlation id, or `"-"` when called outside a
/// captured request (e.g. the background models fetch).
pub fn current_request_id() -> String {
    REQUEST_ID
        .try_with(|id| id.clone())
        .unwrap_or_else(|_| "-".to_string())
}

/// Current Unix time in milliseconds (0 if the clock predates the epoch).
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// A single observable moment in a request's life, streamed to the console.
///
/// Serialized with an internal `kind` tag (`"inbound"` / `"upstream"`) so the
/// browser can switch on it directly.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DebugEvent {
    /// A request exactly as the gateway received it, before any adapter runs.
    Inbound {
        /// Correlation id shared with the matching `Upstream` event.
        id: String,
        /// Capture time (epoch milliseconds).
        ts_ms: u128,
        /// HTTP method, e.g. `POST`.
        method: String,
        /// Request path, e.g. `/v1/chat/completions`.
        path: String,
        /// Inbound headers as `[name, value]` pairs.
        headers: Vec<[String; 2]>,
        /// Inbound body: parsed JSON when possible, otherwise a raw string.
        body: Value,
    },
    /// The request the gateway is about to send to the Raycast backend.
    Upstream {
        /// Correlation id shared with the originating `Inbound` event.
        id: String,
        /// Capture time (epoch milliseconds).
        ts_ms: u128,
        /// HTTP method, always `POST` for chat completions.
        method: String,
        /// Absolute upstream URL.
        url: String,
        /// Upstream request headers (signature, timestamp, auth, ...) as pairs.
        headers: Vec<[String; 2]>,
        /// Upstream body: parsed JSON when possible, otherwise a raw string.
        body: Value,
    },
}

/// Cheap-to-clone handle to the debug event broadcast channel.
///
/// Cloning yields another sender for the same channel; a fresh
/// [`DebugBus::default`]/[`DebugBus::new`] creates an independent channel.
#[derive(Clone)]
pub struct DebugBus {
    tx: broadcast::Sender<DebugEvent>,
}

impl Default for DebugBus {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugBus {
    /// Create a new bus with no subscribers yet.
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Publish an event. When no console is connected the event is silently
    /// dropped (a broadcast send with no receivers is a no-op here).
    pub fn publish(&self, event: DebugEvent) {
        let _ = self.tx.send(event);
    }

    /// Subscribe to future events. Used by the SSE endpoint.
    pub fn subscribe(&self) -> broadcast::Receiver<DebugEvent> {
        self.tx.subscribe()
    }
}

/// Parse bytes as JSON for pretty display, falling back to a lossy UTF-8 string
/// value when the body isn't valid JSON. Shared by both capture sites.
pub fn body_to_value(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
}
