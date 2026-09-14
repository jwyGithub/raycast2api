//! Request-correlation support for the request-log store.
//!
//! [`REQUEST_ID`] is a task-local correlation id set by the `debug_capture`
//! middleware; the upstream client and the SSE bridge use it to attribute
//! their log writes to the originating inbound request — without threading an
//! id through every handler and pipeline signature.

use serde_json::Value;

tokio::task_local! {
    /// Correlation id for the request currently being handled. Set by the
    /// `debug_capture` middleware so the Raycast client can name its upstream
    /// dump after the originating inbound one — without threading an id through
    /// every handler and pipeline signature.
    pub static REQUEST_ID: String;
}

/// The current request's correlation id, or `"-"` when called outside a
/// captured request (e.g. the background models fetch).
pub fn current_request_id() -> String {
    REQUEST_ID
        .try_with(|id| id.clone())
        .unwrap_or_else(|_| "-".to_string())
}

/// Parse bytes as JSON for pretty display, falling back to a lossy UTF-8 string
/// value when the body isn't valid JSON. Shared by both capture sites.
pub fn body_to_value(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
}
