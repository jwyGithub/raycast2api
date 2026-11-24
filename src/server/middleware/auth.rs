use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;

use crate::error::GatewayError;
use crate::key::invalid_key;

use super::super::state::SharedState;

/// Management accepts a configured API key or a dashboard session. Inference
/// keeps API-key authentication and never accepts a dashboard cookie.
///
/// Two auth header styles are accepted, matching the client protocols the
/// gateway fronts:
/// - OpenAI style: `Authorization: Bearer <key>`
/// - Anthropic style: `x-api-key: <key>` (what Claude Code / Anthropic SDKs send)
pub async fn auth(
    State(state): State<SharedState>,
    mut request: Request,
    next: Next,
) -> Result<Response, GatewayError> {
    let path = request.uri().path();
    if path == "/health" || path == "/ready" {
        return Ok(next.run(request).await);
    }

    let key = extract_api_key(request.headers());
    if key.as_deref() == Some(state.settings.api_key.as_str()) {
        return Ok(next.run(request).await);
    }

    let is_management = [
        "/v1/accounts/",
        "/v1/logs/",
        "/v1/models/",
        "/v1/statistics/",
        "/v1/api-keys/",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix));
    if is_management {
        if key
            .as_deref()
            .is_some_and(|key| state.api_keys.contains(key))
        {
            return Err(invalid_key());
        }
        // Keep the session's own "登录已失效/已过期，请重新登录" message.
        crate::server::session::verify(&state, request.headers()).await?;
    } else if matches!(
        (request.method(), path),
        (
            &Method::POST,
            "/v1/chat/completions" | "/v1/responses" | "/v1/messages"
        ) | (&Method::GET, "/v1/models")
    ) {
        let id = state
            .api_keys
            .verify(key.as_deref().ok_or_else(invalid_key)?)?;
        request.extensions_mut().insert(id);
    } else {
        return Err(invalid_key());
    }

    Ok(next.run(request).await)
}

/// Keep the existing catch-all response for unrelated callers.
pub(in crate::server) async fn guard_unknown_route(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Result<Response, GatewayError> {
    if extract_api_key(request.headers()).is_some_and(|key| state.api_keys.contains(&key)) {
        return Err(invalid_key());
    }
    Ok(next.run(request).await)
}

/// Extract the caller's API key from either auth header style. OpenAI-style
/// `Authorization: Bearer` wins if both are present (it is the more explicit
/// form); Anthropic-style `x-api-key` is the fallback.
pub(super) fn extract_api_key(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_from(pairs: &[(&str, &str)]) -> Option<String> {
        let mut headers = axum::http::HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        extract_api_key(&headers)
    }

    #[test]
    fn bearer_header_is_recognized() {
        assert_eq!(
            key_from(&[("authorization", "Bearer sk-123")]),
            Some("sk-123".into())
        );
    }

    #[test]
    fn anthropic_x_api_key_is_recognized() {
        assert_eq!(key_from(&[("x-api-key", "sk-123")]), Some("sk-123".into()));
    }

    #[test]
    fn bearer_wins_over_x_api_key() {
        assert_eq!(
            key_from(&[
                ("authorization", "Bearer openai-key"),
                ("x-api-key", "anthropic-key")
            ]),
            Some("openai-key".into())
        );
    }

    #[test]
    fn missing_or_non_bearer_authorization_falls_through() {
        // 无头
        assert_eq!(key_from(&[]), None);
        // Authorization 存在但不是 Bearer 形式 → 落到 x-api-key(缺失) → None
        assert_eq!(key_from(&[("authorization", "Basic abc")]), None);
        // 非 Bearer + x-api-key → 用 x-api-key
        assert_eq!(
            key_from(&[("authorization", "Basic abc"), ("x-api-key", "k")]),
            Some("k".into())
        );
    }
}
