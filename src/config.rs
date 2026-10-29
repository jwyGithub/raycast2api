//! Static constants and runtime [`Settings`] for the gateway.
//!
//! Static values mirror the reference TypeScript implementation in
//! `source/src/config.ts`. Runtime settings are read from a JSON config file
//! or environment variables as fallback.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

/// Endpoint that streams AI chat completions as Server-Sent Events.
pub const RAYCAST_API_URL: &str = "https://backend.raycast.com/api/v1/ai/chat_completions";

/// Endpoint listing the models available to the account.
pub const RAYCAST_MODELS_URL: &str = "https://backend.raycast.com/api/v1/ai/models";

/// Endpoint returning the authenticated account's profile + subscription.
pub const RAYCAST_ME_URL: &str = "https://backend.raycast.com/api/v1/me";

/// Endpoint returning the account's AI credit balance.
pub const RAYCAST_AI_CREDITS_URL: &str = "https://backend.raycast.com/api/v1/ai/credits";

/// User-Agent string sent with every upstream request (matches the captured client).
pub const USER_AGENT: &str = "Raycast/1.94.2 (macOS Version 15.3.2 (Build 24D81))";

/// Default OpenAI-facing model id used when a request omits `model`.
pub const DEFAULT_MODEL_ID: &str = "openai-gpt-4o-mini";

/// Fallback provider when a requested model id can't be resolved.
pub const DEFAULT_PROVIDER: &str = "openai";

/// Fallback upstream model when a requested model id can't be resolved.
pub const DEFAULT_INTERNAL_MODEL: &str = "gpt-4o-mini";

/// 限额账号冷却缺省时长(秒):上游未给 `retry_after` 时使用。
pub const DEFAULT_COOLDOWN_SECS: u64 = 600;

/// 冷却时长下限(秒):防止上游异常值导致账号长时间不可用。
pub const MIN_COOLDOWN_SECS: u64 = 60;

/// 冷却时长上限(秒):封顶一天。
pub const MAX_COOLDOWN_SECS: u64 = 24 * 60 * 60;

/// JSON configuration file structure.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConfigFile {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub api_key: Option<String>,
    pub raycast_device_id: Option<String>,
    pub raycast_signature_secret: Option<String>,
    pub show_advanced: Option<bool>,
    pub include_deprecated: Option<bool>,
    /// SQLite 存储库路径,默认 `store/store.db`。
    pub accounts_db: Option<String>,
    /// 上游代理链(可选):`http://`/`https://`/`socks5://` URL 列表。上游请求
    /// 直连失败(网络错误)时按顺序逐个换代理重试;缺省/空 = 纯直连。
    #[serde(default, alias = "proxies")]
    pub proxies: Option<Vec<String>>,
    /// Client-facing model id → gateway model id (e.g. `"gpt-6-astra"` →
    /// `"openai-gpt-6-astra"`), applied before the upstream model lookup.
    #[serde(default, alias = "modelOverrides")]
    pub model_overrides: Option<HashMap<String, String>>,
}

/// Runtime configuration, loaded once at startup via [`Settings::from_config`].
#[derive(Clone, Debug)]
pub struct Settings {
    /// Raycast device id (`X-Raycast-DeviceId`, also folded into the signature).
    pub raycast_device_id: String,
    /// Signing secret; `None` uses the built-in default secret.
    pub raycast_signature_secret: Option<String>,
    /// Gateway's own API key. When `None`, the gateway accepts all callers.
    pub api_key: Option<String>,
    /// Primary address the HTTP server binds to (IPv4 or IPv6).
    pub bind_addr: SocketAddr,
    /// Dual-stack: secondary address for dual IPv4/IPv6 binding. `None` if single-stack.
    pub bind_addr_dual: Option<SocketAddr>,
    /// Include premium ("better AI") models in `/v1/models`. Default `true`.
    pub show_advanced: bool,
    /// Include deprecated models in `/v1/models`. Default `true`.
    pub include_deprecated: bool,
    /// Client-facing model id → gateway model id, applied before the upstream
    /// model lookup. Requests naming an aliased id are sent as the mapped id.
    pub model_overrides: HashMap<String, String>,
    /// SQLite 账号库路径(账号表与后续模块共用)。
    pub accounts_db: String,
    /// 上游代理链:直连失败后按顺序重试的代理 URL;空 = 纯直连。
    pub proxies: Vec<String>,
}

impl Settings {
    /// Read configuration from a JSON file, with environment variables as fallback.
    /// Tries to load from `config.json` first; if not found, falls back to environment.
    pub fn from_config() -> Result<Self, String> {
        // Try loading from config.json
        if let Ok(config) = Self::load_config_file("config.json") {
            return Self::from_config_file(config);
        }

        // Fallback to environment variables
        Self::from_env()
    }

    /// Load configuration from a JSON file.
    fn load_config_file(path: &str) -> Result<ConfigFile, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read config file '{path}': {e}"))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("failed to parse config file '{path}': {e}"))
    }

    /// Create Settings from a ConfigFile, with environment variable overrides.
    fn from_config_file(config: ConfigFile) -> Result<Self, String> {
        let raycast_device_id = env_opt("RAYCAST_DEVICE_ID")
            .or(config.raycast_device_id)
            .ok_or_else(|| "missing RAYCAST_DEVICE_ID".to_string())?;

        let accounts_db = env_opt("ACCOUNTS_DB")
            .or(config.accounts_db)
            .unwrap_or_else(|| "store/store.db".to_string());

        let host = env_opt("HOST")
            .or(config.host)
            .unwrap_or_else(|| "0.0.0.0".to_string());

        let port: u16 = match env_opt("PORT") {
            Some(p) => p.parse().map_err(|_| format!("invalid PORT: {p}"))?,
            None => config.port.unwrap_or(8787),
        };

        // Parse primary address
        let addr_string = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };

        let bind_addr: SocketAddr = addr_string
            .parse()
            .map_err(|e| format!("invalid HOST/PORT '{addr_string}': {e} (HOST must be an IP)"))?;

        // Dual-stack binding: if primary is IPv4 "0.0.0.0", also bind to IPv6 "[::]"
        let bind_addr_dual = if bind_addr.ip() == IpAddr::from([0, 0, 0, 0]) {
            Some(SocketAddr::new(
                IpAddr::from([0, 0, 0, 0, 0, 0, 0, 0]),
                port,
            ))
        } else {
            None
        };

        let proxies = parse_proxies(config.proxies, env_opt("PROXIES"))?;

        Ok(Self {
            raycast_device_id,
            raycast_signature_secret: env_opt("RAYCAST_SIGNATURE_SECRET")
                .or(config.raycast_signature_secret),
            api_key: env_opt("API_KEY").or(config.api_key),
            bind_addr,
            bind_addr_dual,
            show_advanced: env_flag("ADVANCED", config.show_advanced.unwrap_or(true)),
            include_deprecated: env_flag(
                "INCLUDE_DEPRECATED",
                config.include_deprecated.unwrap_or(true),
            ),
            model_overrides: config.model_overrides.unwrap_or_default(),
            accounts_db,
            proxies,
        })
    }

    /// Read configuration from the environment. Returns an error naming the
    /// first missing/invalid required variable.
    fn from_env() -> Result<Self, String> {
        let raycast_device_id = env_required("RAYCAST_DEVICE_ID")?;

        let accounts_db = env_opt("ACCOUNTS_DB").unwrap_or_else(|| "store/store.db".to_string());

        let host = env_opt("HOST").unwrap_or_else(|| "0.0.0.0".to_string());
        let port: u16 = match env_opt("PORT") {
            Some(p) => p.parse().map_err(|_| format!("invalid PORT: {p}"))?,
            None => 8787,
        };

        // Parse primary address
        let addr_string = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };

        let bind_addr: SocketAddr = addr_string
            .parse()
            .map_err(|e| format!("invalid HOST/PORT '{addr_string}': {e} (HOST must be an IP)"))?;

        // Dual-stack binding: if primary is IPv4 "0.0.0.0", also bind to IPv6 "[::]"
        let bind_addr_dual = if bind_addr.ip() == IpAddr::from([0, 0, 0, 0]) {
            Some(SocketAddr::new(
                IpAddr::from([0, 0, 0, 0, 0, 0, 0, 0]),
                port,
            ))
        } else {
            None
        };

        let proxies = parse_proxies(None, env_opt("PROXIES"))?;

        Ok(Self {
            raycast_device_id,
            raycast_signature_secret: env_opt("RAYCAST_SIGNATURE_SECRET"),
            api_key: env_opt("API_KEY"),
            bind_addr,
            bind_addr_dual,
            show_advanced: env_flag("ADVANCED", true),
            include_deprecated: env_flag("INCLUDE_DEPRECATED", true),
            // No env-var form for a map; aliases are config-file only.
            model_overrides: HashMap::new(),
            accounts_db,
            proxies,
        })
    }

    /// The signing secret as an `Option<&str>`, ready for the signature code.
    pub fn signature_secret(&self) -> Option<&str> {
        self.raycast_signature_secret.as_deref()
    }
}

/// 解析代理配置:接受 config.json 的数组或 `PROXIES` 环境变量的逗号分隔串,
/// 去空白项,校验 `http://` / `https://` / `socks5://` 前缀。写错配置在启动时
/// 就报错,而不是第一次网络抖动时才发现代理根本没生效。
fn parse_proxies(
    from_file: Option<Vec<String>>,
    from_env: Option<String>,
) -> Result<Vec<String>, String> {
    let list = match from_env {
        // 环境变量覆盖文件,与 device_id/api_key 等的优先级一致。
        Some(s) => s.split(',').map(|p| p.trim().to_string()).collect(),
        None => from_file.unwrap_or_default(),
    };
    let proxies = list
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    for p in &proxies {
        let ok = p.starts_with("http://")
            || p.starts_with("https://")
            || p.starts_with("socks5://");
        if !ok {
            return Err(format!(
                "invalid proxy '{p}': must start with http://, https:// or socks5://"
            ));
        }
    }
    Ok(proxies)
}

#[cfg(test)]
mod proxy_tests {
    use super::*;

    #[test]
    fn parse_proxies_validates_scheme_and_skips_empty() {
        assert!(parse_proxies(None, None).unwrap().is_empty());
        assert!(parse_proxies(Some(vec![]), None).unwrap().is_empty());
        assert!(parse_proxies(None, Some("  ,,".into())).unwrap().is_empty());

        let ok = parse_proxies(
            Some(vec![
                "http://127.0.0.1:7897".into(),
                "socks5://user:pass@1.2.3.4:1080".into(),
                "https://proxy.example.com".into(),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(ok.len(), 3);

        // 环境变量逗号串切分 + 去空白,并覆盖文件配置。
        let env = parse_proxies(
            Some(vec!["http://file.example:1".into()]),
            Some(" http://a:1 , socks5://b:2 ,".into()),
        )
        .unwrap();
        assert_eq!(env, vec!["http://a:1", "socks5://b:2"]);

        let err = parse_proxies(Some(vec!["ftp://1.2.3.4".into()]), None).unwrap_err();
        assert!(err.contains("invalid proxy"));
    }
}

fn env_required(key: &str) -> Result<String, String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(format!("missing required environment variable {key}")),
    }
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// Boolean flag from the environment. Matches the reference semantics: any value
/// other than the literal `"false"` (case-insensitive) is treated as enabled.
fn env_flag(key: &str, default: bool) -> bool {
    match env_opt(key) {
        Some(v) => !v.eq_ignore_ascii_case("false"),
        None => default,
    }
}
