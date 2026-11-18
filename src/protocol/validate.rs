//! Request-level validation shared by the inbound adapters (②).
//!
//! Every protocol runs the same checks and gets the same 400 wording, so a
//! client sees one error no matter which endpoint it hit.

use crate::error::GatewayError;

/// `model` selects the upstream provider/model, so every protocol requires it:
/// missing, empty or whitespace-only is a 400 (there is no default model).
pub(crate) fn require_model(model: Option<String>) -> Result<String, GatewayError> {
    model
        .filter(|m| !m.trim().is_empty())
        .ok_or_else(|| GatewayError::BadRequest("'model' is required and must not be empty".into()))
}

/// The request must carry at least one conversational turn. `field` is the
/// protocol's own name for it (`messages`, `input`).
pub(crate) fn require_non_empty<T>(field: &str, items: &[T]) -> Result<(), GatewayError> {
    if items.is_empty() {
        return Err(GatewayError::BadRequest(format!(
            "Missing or invalid '{field}' field"
        )));
    }
    Ok(())
}
