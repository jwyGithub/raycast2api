//! Static constants and runtime [`Settings`] for the gateway.
//!
//! Static values mirror the reference TypeScript implementation in
//! `source/src/config.ts`. Runtime settings are read from `config.json` only;
//! environment variables are not consulted.

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

/// Endpoint returning credit usage details (buckets + grants) for a time range.
pub const RAYCAST_AI_CREDITS_DETAILS_URL: &str =
    "https://backend.raycast.com/api/v1/ai/credits/details";

/// Endpoint returning trial status (limits/organizations) for trial accounts.
pub const RAYCAST_TRIAL_STATUS_URL: &str = "https://backend.raycast.com/api/v1/me/trial_status";

/// Endpoint issuing/refreshing OAuth tokens (access_token 换新).
pub const OAUTH_TOKEN_URL: &str = "https://backend.raycast.com/oauth/token";

/// Raycast 桌面客户端的公开 OAuth client id(逆向自 backend/index.mjs;
/// 与 scripts/ 下的注册/刷新脚本硬编码值一致 —— 上游变更时同改)。
pub const OAUTH_CLIENT_ID: &str = "FRsHICIAlyPB_v2m4tfVqHtVUS40Ieco_da0Y6zBwgA";

/// User-Agent string sent with every upstream request (matches the captured
/// client). 必须是近期真实客户端版本:上游按客户端版本判定 Better AI 资格,
/// 过老的 UA(如 1.94.2)会让所有 Advanced AI 档模型被降级(见
/// doc/model-degradation.md 的隔离测试)。
pub const USER_AGENT: &str = "Raycast/2.11.0.0 (x-Windows-amd64 10.0.26100.9711)";

/// Default OpenAI-facing model id used when a request omits `model`.
pub const DEFAULT_MODEL_ID: &str = "openai-gpt-6.1-sol";

/// Fallback provider when a requested model id can't be resolved.
pub const DEFAULT_PROVIDER: &str = "openai";

/// Fallback upstream model when a requested model id can't be resolved.
pub const DEFAULT_INTERNAL_MODEL: &str = "gpt-6.1-sol";

/// 限额账号冷却缺省时长(秒):上游未给 `retry_after` 时使用。
pub const DEFAULT_COOLDOWN_SECS: u64 = 600;

/// 冷却时长下限(秒):防止上游异常值导致账号长时间不可用。
pub const MIN_COOLDOWN_SECS: u64 = 60;

/// 冷却时长上限(秒):封顶一天。
pub const MAX_COOLDOWN_SECS: u64 = 24 * 60 * 60;

/// JSON configuration file structure.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ConfigFile {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub api_key: Option<String>,
    pub dashboard_password: Option<String>,
    pub raycast_device_id: Option<String>,
    pub raycast_signature_secret: Option<String>,
    /// SQLite 存储库路径,默认 `store/store.db`。
    pub accounts_db: Option<String>,
    /// 上游超时（秒），必须大于零，默认 20。
    pub upstream_timeout_secs: Option<std::num::NonZeroU64>,
    /// 上游代理链(可选):`http://`/`https://`/`socks5://` URL 列表。
    /// `proxies_enabled` 决定代理是首选还是回退通道;缺省/空 = 纯直连。
    #[serde(default, alias = "proxies")]
    pub proxies: Option<Vec<String>>,
    /// 代理通道模式:`true` = 代理优先,全挂回退直连;`false`(缺省) =
    /// 直连优先,网络失败换代理。两种模式都要求代理链非空才生效。
    #[serde(default, alias = "proxiesEnabled")]
    pub proxies_enabled: Option<bool>,
    /// Client-facing model id → gateway model id (e.g. `"gpt-6-astra"` →
    /// `"openai-gpt-6-astra"`), applied before the upstream model lookup.
    #[serde(default, alias = "modelOverrides")]
    pub model_overrides: Option<HashMap<String, String>>,
}

/// Runtime configuration, loaded once at startup via [`Settings::from_path`].
#[derive(Clone, Debug)]
pub struct Settings {
    /// Raycast device id (`X-Raycast-DeviceId`, also folded into the signature).
    pub raycast_device_id: String,
    /// Signing secret for `X-Raycast-Signature-v2` (required, no built-in default).
    pub raycast_signature_secret: String,
    /// Gateway's own API key. When `None`, the gateway accepts all callers.
    pub api_key: Option<String>,
    /// Console login password. When absent, dashboard login is disabled.
    pub dashboard_password: Option<String>,
    /// Primary address the HTTP server binds to (IPv4 or IPv6).
    pub bind_addr: SocketAddr,
    /// Dual-stack: secondary address for dual IPv4/IPv6 binding. `None` if single-stack.
    pub bind_addr_dual: Option<SocketAddr>,
    /// Client-facing model id → gateway model id, applied before the upstream
    /// model lookup. Requests naming an aliased id are sent as the mapped id.
    pub model_overrides: HashMap<String, String>,
    /// SQLite 账号库路径(账号表与后续模块共用)。
    pub accounts_db: String,
    pub upstream_timeout: std::time::Duration,
    /// 上游代理链:按顺序尝试的代理 URL;空 = 纯直连。
    pub proxies: Vec<String>,
    /// 代理通道模式:`true` = 代理优先(全挂回退直连),`false` = 直连优先
    /// (网络失败换代理)。
    pub proxies_enabled: bool,
}

impl Settings {
    /// Read and validate the JSON configuration file at `path`.
    pub fn from_path(path: &std::path::Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "config.json not found (copy config.example.json to config.json)".to_string()
            } else {
                format!("failed to read config.json: {e}")
            }
        })?;
        Self::from_config_file(
            serde_json::from_str(&content).map_err(|e| format!("invalid config.json: {e}"))?,
        )
    }

    /// Create Settings from a parsed ConfigFile.
    fn from_config_file(config: ConfigFile) -> Result<Self, String> {
        let raycast_device_id = non_empty(config.raycast_device_id)
            .ok_or_else(|| "missing raycast_device_id in config.json".to_string())?;
        let raycast_signature_secret = non_empty(config.raycast_signature_secret)
            .ok_or_else(|| "missing raycast_signature_secret in config.json".to_string())?;

        let accounts_db = config
            .accounts_db
            .unwrap_or_else(|| "store/store.db".to_string());

        let host = config.host.unwrap_or_else(|| "0.0.0.0".to_string());
        let port = config.port.unwrap_or(8787);

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

        let proxies = parse_proxies(config.proxies)?;
        let proxies_enabled = config.proxies_enabled.unwrap_or(false);

        Ok(Self {
            raycast_device_id,
            raycast_signature_secret,
            api_key: non_empty(config.api_key),
            dashboard_password: non_empty(config.dashboard_password),
            bind_addr,
            bind_addr_dual,
            model_overrides: config.model_overrides.unwrap_or_default(),
            upstream_timeout: std::time::Duration::from_secs(
                config
                    .upstream_timeout_secs
                    .map_or(20, std::num::NonZeroU64::get),
            ),
            accounts_db,
            proxies,
            proxies_enabled,
        })
    }
}

/// 解析代理配置:去空项,校验 `http://` / `https://` / `socks5://` 前缀。
/// 写错配置在启动时就报错,而不是第一次网络抖动时才发现代理根本没生效。
fn parse_proxies(from_file: Option<Vec<String>>) -> Result<Vec<String>, String> {
    let proxies = from_file
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    for p in &proxies {
        let ok =
            p.starts_with("http://") || p.starts_with("https://") || p.starts_with("socks5://");
        if !ok {
            return Err(format!(
                "invalid proxy '{p}': must start with http://, https:// or socks5://"
            ));
        }
    }
    Ok(proxies)
}

/// Treat an empty string in `config.json` the same as an absent key.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod proxy_tests {
    use super::*;

    #[test]
    fn dashboard_password_preserves_exact_value_and_disables_login_when_empty() {
        for (password, expected) in [
            (None, None),
            (Some(""), None),
            (Some(" 控制台 password "), Some(" 控制台 password ")),
        ] {
            let settings = Settings::from_config_file(ConfigFile {
                raycast_device_id: Some("device".into()),
                raycast_signature_secret: Some("secret".into()),
                dashboard_password: password.map(str::to_owned),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(settings.dashboard_password.as_deref(), expected);
        }
    }

    #[test]
    fn upstream_timeout_defaults_and_accepts_positive_seconds_only() {
        for (json, expected) in [
            (
                r#"{"raycast_device_id":"test","raycast_signature_secret":"s"}"#,
                20,
            ),
            (
                r#"{"raycast_device_id":"test","raycast_signature_secret":"s","upstream_timeout_secs":45}"#,
                45,
            ),
        ] {
            let config = serde_json::from_str(json).unwrap();
            assert_eq!(
                Settings::from_config_file(config)
                    .unwrap()
                    .upstream_timeout
                    .as_secs(),
                expected
            );
        }
        for value in ["0", "-1", "1.5", "\"20\""] {
            assert!(
                serde_json::from_str::<ConfigFile>(&format!(
                    "{{\"upstream_timeout_secs\":{value}}}"
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn malformed_configuration_is_rejected() {
        let path = std::env::temp_dir().join(format!(
            "r2a-invalid-config-{}.json",
            crate::util::new_ulid()
        ));
        std::fs::write(&path, "{").unwrap();
        let error = Settings::from_path(&path).unwrap_err();
        std::fs::remove_file(path).unwrap();
        assert!(error.contains("invalid config.json"));
    }

    #[test]
    fn missing_config_file_is_an_error() {
        let path = std::env::temp_dir().join(format!(
            "r2a-missing-config-{}.json",
            crate::util::new_ulid()
        ));
        let error = Settings::from_path(&path).unwrap_err();
        assert!(error.contains("config.json not found"));
    }

    #[test]
    fn device_id_and_signature_secret_are_required() {
        for (json, missing) in [
            (r#"{"raycast_signature_secret":"s"}"#, "raycast_device_id"),
            (r#"{"raycast_device_id":"d"}"#, "raycast_signature_secret"),
            (
                r#"{"raycast_device_id":"d","raycast_signature_secret":""}"#,
                "raycast_signature_secret",
            ),
        ] {
            let error =
                Settings::from_config_file(serde_json::from_str(json).unwrap()).unwrap_err();
            assert!(error.contains(missing), "{json} -> {error}");
        }
    }

    #[test]
    fn parse_proxies_validates_scheme_and_skips_empty() {
        assert!(parse_proxies(None).unwrap().is_empty());
        assert!(parse_proxies(Some(vec![])).unwrap().is_empty());
        assert!(parse_proxies(Some(vec!["".into()])).unwrap().is_empty());

        let ok = parse_proxies(Some(vec![
            "http://127.0.0.1:7897".into(),
            "socks5://user:pass@1.2.3.4:1080".into(),
            "https://proxy.example.com".into(),
        ]))
        .unwrap();
        assert_eq!(ok.len(), 3);

        let err = parse_proxies(Some(vec!["ftp://1.2.3.4".into()])).unwrap_err();
        assert!(err.contains("invalid proxy"));
    }
}
