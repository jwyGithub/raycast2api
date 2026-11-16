//! Dashboard login, session validation, and logout.

use axum::Json;
use axum::extract::State;
use axum::http::{
    HeaderMap, HeaderValue,
    header::{CACHE_CONTROL, SET_COOKIE},
};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::error::GatewayError;
use crate::server::session;
use crate::server::state::SharedState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginRequest {
    pub dashboard_password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    expires_in: u64,
}

/// `POST /v1/login` is public; only the configured console password is accepted.
pub async fn login(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> Result<impl IntoResponse, GatewayError> {
    let expected = state
        .settings
        .dashboard_password
        .as_deref()
        .filter(|password| !password.is_empty())
        .ok_or_else(|| {
            GatewayError::Config(
                "未配置控制台密码，请在 config.json 中设置 dashboard_password 后重启服务。".into(),
            )
        })?;
    if request.dashboard_password != expected {
        return Err(GatewayError::Unauthorized(
            "控制台密码错误，请重新输入。".into(),
        ));
    }

    session::revoke(&state, &headers).await?;
    let token = session::create(&state).await?;
    Ok((
        [
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (SET_COOKIE, session::cookie(&headers, &token)),
        ],
        Json(LoginResponse {
            expires_in: session::SESSION_TTL_SECS,
        }),
    ))
}

pub async fn session(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, GatewayError> {
    session::verify(&state, &headers).await?;
    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"authenticated": true})),
    ))
}

pub async fn logout(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, GatewayError> {
    session::verify(&state, &headers).await?;
    session::revoke(&state, &headers).await?;
    Ok((
        [
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (SET_COOKIE, session::cookie(&headers, "")),
        ],
        Json(serde_json::Value::Null),
    ))
}
