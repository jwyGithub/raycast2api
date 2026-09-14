//! raycast2api — an OpenAI-compatible gateway that relays to Raycast's AI
//! backend.
//!
//! The crate is organized into layers so it stays easy to extend:
//!
//! * [`account`]   — the Raycast account domain (store, import, refresh).
//! * [`config`]    — static endpoints and runtime [`config::Settings`].
//! * [`core`]      — the canonical, protocol- and provider-agnostic model.
//! * [`db`]        — shared SQLite infrastructure (connection management).
//! * [`debug`]     — request-dump support (files under `DEBUG_DUMP_DIR`).
//! * [`error`]     — the unified [`error::GatewayError`] (OpenAI-style output).
//! * [`signature`] — the `X-Raycast-Signature-v2` computation.
//! * [`raycast`]   — the upstream provider (async client, wire types, SSE).
//! * [`protocol`]  — the client-protocol adapters (OpenAI Chat Completions,
//!   Anthropic Messages, OpenAI Responses).
//! * [`server`]    — the HTTP layer (state, middleware, handlers, router).
//! * [`util`]      — small shared helpers.

pub mod account;
pub mod config;
pub mod core;
pub mod db;
pub mod debug;
pub mod error;
pub mod protocol;
pub mod raycast;
pub mod server;
pub mod signature;
pub mod util;
