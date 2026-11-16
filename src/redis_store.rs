//! Shared Redis connection; application settings remain in config.json.

use std::time::Duration;

use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::{IntoConnectionInfo, RedisConnectionInfo};

pub async fn connect_from_env() -> Result<Option<ConnectionManager>, String> {
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
    ConnectionManager::new_with_config(client, config)
        .await
        .map(Some)
        .map_err(|error| format!("failed to connect to Redis: {error}"))
}
