//! The management console's static files.
//!
//! Everything under `src/page/` is served at its relative path (`index.html`
//! also at `/`), so dropping a file into that directory is all it takes to
//! publish it. Release builds embed the files in the binary; debug builds read
//! them from disk, so edits show up on refresh without recompiling. This is the
//! router fallback and sits outside the auth layer: the page must load before
//! the user can log in.

use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "src/page"]
#[exclude = "*.DS_Store"]
struct Assets;

/// Content only changes with a new binary and URLs are unversioned, so assets
/// are revalidated with a content ETag (`no-cache` + 304) rather than given a
/// max-age: a stale module paired with a new one after an upgrade would break
/// the page.
pub async fn serve(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = match uri.path().trim_start_matches('/') {
        "" => "index.html",
        path => path,
    };
    let Some(file) = Assets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !matches!(method, Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    let etag = format!("\"{}\"", hex::encode(file.metadata.sha256_hash()));
    let cache = [
        (header::ETAG, etag.clone()),
        (header::CACHE_CONTROL, "no-cache".to_owned()),
    ];
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains(&etag));
    if fresh {
        return (StatusCode::NOT_MODIFIED, cache).into_response();
    }
    let content_type = [(header::CONTENT_TYPE, file.metadata.mimetype().to_owned())];
    (cache, content_type, file.data).into_response()
}
