//! raycast2api — an OpenAI-compatible gateway that relays to Raycast's AI
//! backend.
//!
//! The crate is organized into layers so it stays easy to extend:
//!
//! * [`account`]   — the Raycast account domain (store, import, refresh).
//! * [`config`]    — static endpoints and runtime [`config::Settings`].
//! * [`core`]      — the canonical, protocol- and provider-agnostic model.
//! * [`storage`]   — shared persistence infrastructure (SQLite connection,
//!   diesel schema, Redis connection).
//! * [`log::context`]     — request-correlation support for the request-log store.
//! * [`error`]     — the unified [`error::GatewayError`] (OpenAI-style output).
//! * [`log`]       — request-log storage (inbound/upstream/response, 7-day retention).
//! * [`model`]    — console-managed model aliases (client model id → gateway model id).
//! * [`signature`] — the `X-Raycast-Signature-v2` computation.
//! * [`proxies`]   — upstream proxy mode, startup validation and channel order.
//! * [`raycast`]   — the upstream provider (async client, wire types, SSE).
//! * [`protocol`]  — the client-protocol adapters (OpenAI Chat Completions,
//!   Anthropic Messages, OpenAI Responses).
//! * [`server`]    — the HTTP layer (state, middleware, handlers, router).
//! * [`util`]      — small shared helpers.

pub mod account;
pub mod config;
pub mod core;
pub mod error;
mod key;
pub mod log;
pub mod model;
pub mod protocol;
pub mod proxies;
pub mod raycast;
pub mod server;
pub mod signature;
pub mod statistics;
pub mod storage;
pub mod util;
