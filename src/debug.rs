//! Request-dump support for offline analysis.
//!
//! When `DEBUG_DUMP_DIR` is set, the gateway writes every inference request's
//! inbound body, the upstream request it issues, and the upstream SSE response
//! to files named by a correlation id (`{id}-inbound.json`,
//! `{id}-upstream.json`, `{id}-upstream-response.ndjson`). The id is threaded
//! through a task-local set by the `debug_capture` middleware, so no handler
//! or pipeline signature has to know this exists. Unset, everything here is a
//! no-op.

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

/// Dump captured traffic to a file under `DEBUG_DUMP_DIR` (no-op when unset).
///
/// The file name is caller-chosen (e.g. `{request_id}-inbound.json`); the
/// directory is created on first write. Enabled by setting the env var once
/// per process.
pub fn dump(name: &str, contents: &str) {
    let Some(dir) = dump_dir() else { return };
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join(name), contents);
}

/// Whether file dumping is enabled, so callers can skip buffering data that
/// nobody will write.
pub fn dump_enabled() -> bool {
    dump_dir().is_some()
}

/// The configured dump directory, read from the env once per process.
fn dump_dir() -> Option<&'static std::path::Path> {
    use std::path::PathBuf;
    use std::sync::OnceLock;

    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| std::env::var("DEBUG_DUMP_DIR").ok().map(PathBuf::from))
        .as_deref()
}
