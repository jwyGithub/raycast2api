//! Shared Redis handle (connection + key prefix); settings remain in config.json.

use std::time::Duration;

use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::{IntoConnectionInfo, RedisConnectionInfo};

/// Best-effort cache operations give up after this long.
const CACHE_TIMEOUT: Duration = Duration::from_millis(100);

/// A Redis connection plus the namespace prefix every key must carry.
#[derive(Clone)]
pub struct RedisKv {
    conn: ConnectionManager,
    prefix: String,
}

impl RedisKv {
    pub fn new(conn: ConnectionManager, prefix: impl Into<String>) -> Self {
        Self {
            conn,
            prefix: prefix.into(),
        }
    }

    /// `{prefix}:{ns}:{id}` — the single place key layout is defined.
    pub fn key(&self, ns: &str, id: &str) -> String {
        format!("{}:{ns}:{id}", self.prefix)
    }

    /// Cheap clone of the underlying connection for callers that need to
    /// surface Redis errors (sessions).
    pub fn conn(&self) -> ConnectionManager {
        self.conn.clone()
    }

    /// Best-effort `MGET`; `None` on timeout or error.
    pub async fn mget(&self, keys: Vec<String>) -> Option<Vec<Option<Vec<u8>>>> {
        let mut conn = self.conn();
        let mut cmd = redis::cmd("MGET");
        cmd.arg(keys);
        match tokio::time::timeout(CACHE_TIMEOUT, cmd.query_async(&mut conn)).await {
            Ok(Ok(values)) => Some(values),
            _ => None,
        }
    }

    /// Best-effort `SET key value EX ttl`; returns whether it succeeded.
    pub async fn set_ex(&self, key: String, value: impl redis::ToRedisArgs, ttl: Duration) -> bool {
        let mut conn = self.conn();
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(value).arg("EX").arg(ttl.as_secs());
        matches!(
            tokio::time::timeout(CACHE_TIMEOUT, cmd.query_async::<()>(&mut conn)).await,
            Ok(Ok(()))
        )
    }
}

pub async fn connect_from_env() -> Result<Option<RedisKv>, String> {
    let read = |name: &str| -> Result<Option<String>, String> {
        match std::env::var(name) {
            Ok(value) => Ok((!value.is_empty()).then_some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => Err(format!("{name} must be valid UTF-8")),
        }
    };
    let Some(host) = read("REDIS_HOST")? else {
        return Ok(None);
    };
    let port = read("REDIS_PORT")?
        .unwrap_or_else(|| "6379".into())
        .parse::<u16>()
        .ok()
        .filter(|port| *port > 0)
        .ok_or("REDIS_PORT must be in 1..=65535")?;
    let db = read("REDIS_DB")?
        .unwrap_or_else(|| "0".into())
        .parse::<i64>()
        .ok()
        .filter(|db| *db >= 0)
        .ok_or("REDIS_DB must be a non-negative integer")?;
    let mut auth = RedisConnectionInfo::default().set_db(db);
    if let Some(username) = read("REDIS_USERNAME")? {
        auth = auth.set_username(username);
    }
    if let Some(password) = read("REDIS_PASSWORD")? {
        auth = auth.set_password(password);
    }
    let info = (host.as_str(), port)
        .into_connection_info()
        .map_err(|_| "invalid REDIS_HOST")?
        .set_redis_settings(auth);
    let client = redis::Client::open(info).map_err(|_| "invalid Redis configuration")?;
    let config = ConnectionManagerConfig::new()
        .set_connection_timeout(Some(Duration::from_secs(3)))
        .set_response_timeout(Some(Duration::from_secs(3)))
        .set_number_of_retries(1);
    let conn = ConnectionManager::new_with_config(client, config)
        .await
        .map_err(|error| format!("failed to connect to Redis: {error}"))?;
    let prefix = read("REDIS_KEY_PREFIX")?.unwrap_or_else(|| "raycast2api".into());
    Ok(Some(RedisKv::new(conn, prefix)))
}
