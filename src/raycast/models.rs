//! Raycast model registry helpers: resolve a client-facing model id to the
//! upstream `(provider, model)` pair, and reason about think/`-reasoning`
//! variants.
//!
//! The fetch + caching of the raw list lives in [`crate::server::state`] (so it
//! can share the app's HTTP client and serve-stale policy); this module holds
//! the pure resolution logic over an already-fetched list.

use super::wire::RaycastRawModel;

/// Suffix Raycast uses to mark a model's reasoning ("think") variant.
pub const REASONING_SUFFIX: &str = "-reasoning";

/// The upstream `(provider, model)` a request will be sent with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub provider: String,
    pub model: String,
}

/// Resolve a client-facing model id without silently selecting another model.
pub fn resolve(models: &[RaycastRawModel], requested_id: &str) -> Option<ResolvedModel> {
    models
        .iter()
        .find(|m| m.id == requested_id)
        .map(|m| ResolvedModel {
            provider: m.provider.clone(),
            model: m.model.clone(),
        })
}

/// Whether a raw model entry is a reasoning ("think") variant.
pub fn is_think_model(model: &RaycastRawModel) -> bool {
    model.model.ends_with(REASONING_SUFFIX) || model.id.ends_with(REASONING_SUFFIX)
}
