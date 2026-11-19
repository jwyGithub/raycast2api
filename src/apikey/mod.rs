//! Managed credentials, admission and usage accounting.
mod store;
pub(crate) mod wire;

pub(crate) use store::ApiKeyStore;

use axum::http::StatusCode;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use crate::error::GatewayError;

#[derive(Clone)]
pub(crate) struct KeyId(pub String);

pub(crate) struct Permit(Arc<AtomicU32>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub(crate) struct Accounting {
    pub store: Arc<ApiKeyStore>,
    pub id: KeyId,
    pub required: bool,
}

pub(crate) fn invalid_key() -> GatewayError {
    GatewayError::Admission {
        status: StatusCode::UNAUTHORIZED,
        code: "invalid_api_key",
        message: "Invalid API key provided.".into(),
    }
}

pub(crate) fn model_not_found(id: &str) -> GatewayError {
    GatewayError::Admission {
        status: StatusCode::NOT_FOUND,
        code: "model_not_found",
        message: format!("Model '{id}' was not found"),
    }
}
