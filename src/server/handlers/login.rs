//! Console password login that returns the configured gateway API key.

use axum::Json;
use axum::extract::State;
use axum::http::header::CACHE_CONTROL;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::error::GatewayError;
use crate::server::state::SharedState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginRequest {
    pub dashboard_password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    api_key: Option<String>,
}

/// `POST /v1/login` is public; only the configured console password is accepted.
pub async fn login(
    State(state): State<SharedState>,
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

    Ok((
        [(CACHE_CONTROL, "no-store")],
        Json(LoginResponse {
            api_key: state.settings.api_key.clone(),
        }),
    ))
}
