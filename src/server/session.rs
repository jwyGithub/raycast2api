//! Signed, revocable dashboard sessions backed exclusively by Redis.

use axum::http::{HeaderMap, HeaderValue, header};
use hmac::{Hmac, KeyInit, Mac};
use redis::AsyncCommands;
use sha2::{Digest, Sha256};

use crate::config::Settings;
use crate::error::GatewayError;

use super::state::AppState;

const COOKIE_NAME: &str = "r2a_dashboard";
pub const SESSION_TTL_SECS: u64 = 24 * 60 * 60;

fn credentials(settings: &Settings) -> Result<(&str, &str), GatewayError> {
    let password = settings
        .dashboard_password
        .as_deref()
        .filter(|s| !s.is_empty());
    let secret = settings
        .dashboard_session_secret
        .as_deref()
        .filter(|s| s.len() >= 32);
    password.zip(secret).ok_or_else(|| GatewayError::Config(
        "请在 config.json 中设置 dashboard_password 和至少 32 字节的 dashboard_session_secret。".into(),
    ))
}

fn signature(settings: &Settings, id: &str) -> Result<Hmac<Sha256>, GatewayError> {
    let (password, secret) = credentials(settings)?;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(b"dashboard-session-v1\0");
    mac.update(id.as_bytes());
    mac.update(password.as_bytes());
    Ok(mac)
}

fn valid_token(settings: &Settings, token: &str) -> bool {
    if token.len() != 129 || token.as_bytes()[64] != b'.' || !token.is_ascii() {
        return false;
    }
    let (id, tag) = (&token[..64], &token[65..]);
    if !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let Ok(tag) = hex::decode(tag) else {
        return false;
    };
    signature(settings, id).is_ok_and(|mac| mac.verify_slice(&tag).is_ok())
}

fn session_key(state: &AppState, token: &str) -> String {
    format!(
        "{}:dashboard:session:{}",
        state.redis_key_prefix,
        hex::encode(Sha256::digest(token.as_bytes()))
    )
}

pub fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| (name == COOKIE_NAME).then_some(value))
}

fn unavailable(error: redis::RedisError) -> GatewayError {
    tracing::error!(%error, "dashboard session Redis operation failed");
    GatewayError::Unavailable("登录会话存储暂不可用，请稍后重试。".into())
}

pub async fn create(state: &AppState) -> Result<String, GatewayError> {
    credentials(&state.settings)?;
    let mut connection = state.redis.clone().ok_or_else(|| {
        GatewayError::Config("Dashboard Session 需要配置 REDIS_HOST 并连接 Redis。".into())
    })?;
    let mut random = [0; 32];
    crate::util::fill_random(&mut random)
        .map_err(|_| GatewayError::Internal("无法生成安全会话。".into()))?;
    let id = hex::encode(random);
    let tag = hex::encode(signature(&state.settings, &id)?.finalize().into_bytes());
    let token = format!("{id}.{tag}");
    connection
        .set_ex::<_, _, ()>(session_key(state, &token), "1", SESSION_TTL_SECS)
        .await
        .map_err(unavailable)?;
    Ok(token)
}

pub async fn verify(state: &AppState, headers: &HeaderMap) -> Result<(), GatewayError> {
    // A non-simple header prevents cross-origin forms and same-site sibling CSRF.
    let token = cookie_token(headers)
        .filter(|token| {
            headers
                .get("x-dashboard-request")
                .is_some_and(|value| value == "1")
                && valid_token(&state.settings, token)
        })
        .ok_or_else(|| GatewayError::Unauthorized("登录已失效，请重新登录。".into()))?;
    let mut connection = state
        .redis
        .clone()
        .ok_or_else(|| GatewayError::Unavailable("登录会话存储未连接。".into()))?;
    let exists: bool = connection
        .exists(session_key(state, token))
        .await
        .map_err(unavailable)?;
    if !exists {
        return Err(GatewayError::Unauthorized(
            "登录已过期，请重新登录。".into(),
        ));
    }
    Ok(())
}

pub async fn revoke(state: &AppState, headers: &HeaderMap) -> Result<(), GatewayError> {
    if let Some(token) = cookie_token(headers).filter(|token| valid_token(&state.settings, token)) {
        let mut connection = state
            .redis
            .clone()
            .ok_or_else(|| GatewayError::Unavailable("登录会话存储未连接。".into()))?;
        connection
            .del::<_, ()>(session_key(state, token))
            .await
            .map_err(unavailable)?;
    }
    Ok(())
}

pub fn cookie(headers: &HeaderMap, token: &str) -> HeaderValue {
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        == Some("https");
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={token}; Path=/v1; HttpOnly; SameSite=Strict; Max-Age={}{}",
        if token.is_empty() {
            0
        } else {
            SESSION_TTL_SECS
        },
        if secure { "; Secure" } else { "" },
    ))
    .expect("session tokens are ASCII hex")
}
