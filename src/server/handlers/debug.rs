//! The local development console: a live request monitor (SSE) plus the static
//! single-page UI that consumes it.
//!
//! Both routes are mounted under `/debug`, *outside* the API-key layer, so the
//! console works in dev without a key. This surface is not meant to be exposed
//! publicly — it streams raw upstream headers (including the bearer token and
//! request signature) to any connected browser.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use futures_util::Stream;
use tokio::sync::broadcast::error::RecvError;

use crate::server::state::SharedState;

/// The single-file console UI, embedded at compile time. The standalone file at
/// `debug/console.html` stays editable for quick tweaks (reload in the browser);
/// this served copy updates when the binary is rebuilt.
const CONSOLE_HTML: &str = include_str!("../../../debug/console.html");

/// `GET /debug/console` — serve the console UI.
pub async fn console() -> impl IntoResponse {
    Html(CONSOLE_HTML)
}

/// `GET /debug/events` — stream captured inbound/upstream events to the console
/// as Server-Sent Events. Each event's JSON payload is one `DebugEvent`.
pub async fn events(
    State(state): State<SharedState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.debug.subscribe();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => match Event::default().json_data(&event) {
                    Ok(sse) => yield Ok(sse),
                    // A DebugEvent always serializes; skip on the off chance it doesn't.
                    Err(_) => continue,
                },
                // The console fell behind and missed events — keep streaming.
                Err(RecvError::Lagged(_)) => continue,
                // The bus was dropped (shutdown) — end the stream.
                Err(RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
