//! Components ④ & ⑦ plus the upstream transport — the **only** Raycast-coupled
//! layer. Everything specific to talking to Raycast's backend lives here: the
//! request [`builder`], the SSE [`parser`], the wire types, the signed
//! [`client`], the low-level SSE byte decoder, and model resolution (the
//! request signature lives in the top-level [`crate::signature`] module).
//! Swapping the upstream provider is a change confined to
//! this module; `core` and the client protocols never see it.

pub mod builder;
pub mod client;
pub mod models;
pub mod parser;
pub mod sse;
pub mod stream;
pub mod wire;
