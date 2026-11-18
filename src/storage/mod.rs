//! Shared persistence infrastructure: no business tables or logic live here,
//! only what the domain stores (`account`, `log`, `statistics`) build on.
//!
//! * [`db`]          — SQLite connection management.
//! * [`schema`]      — the diesel table definitions.
//! * [`redis_store`] — the optional Redis connection.

pub mod db;
pub mod redis_store;
pub mod schema;
