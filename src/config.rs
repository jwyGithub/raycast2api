//! Static constants and runtime [`Settings`] for the gateway.
//!
//! Static values mirror the reference TypeScript implementation in
//! `source/src/config.ts`. Runtime settings are read from `config.json` only;
//! Redis connection settings are read separately from `REDIS_*` environment variables.

use crate::proxies::ProxyMode;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};

/// Endpoint that streams AI chat completions as Server-Sent Events.
pub const RAYCAST_API_URL: &str = "https://backend.raycast.com/api/v1/ai/chat_completions";

/// Endpoint listing the models available to the account.
pub const RAYCAST_MODELS_URL: &str = "https://backend.raycast.com/api/v1/ai/models";

/// Endpoint that registers an attachment upload and returns a presigned
/// direct-upload target (Rails ActiveStorage style, reverse-engineered from
/// the client bundle's `ai/files` module).
pub const RAYCAST_FILES_URL: &str = "https://backend.raycast.com/api/v1/ai/files";

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

/// User-Agent sent with GET requests — the `x-Windows-amd64` variant the real
/// client uses for them (captures 2026-11-13/20). 必须是近期真实客户端版本:
/// 上游按客户端版本判定 Better AI 资格,过老的 UA(如 1.94.2)会让所有
/// Advanced AI 档模型被降级(见 doc/model-degradation.md 的隔离测试)。
pub const USER_AGENT: &str = "Raycast/2.13.1.0 (x-Windows-amd64 10.0.26100.9974)";

/// User-Agent sent with signed POSTs (chat_completions) — the real client
/// swaps in the `x-Windows Version` variant there (captures 2026-11-13/20).
pub const USER_AGENT_POST: &str = "Raycast/2.13.1.0 (x-Windows Version 10.0.26100.9974)";

/// 上游连接池空闲回收时间。reqwest 默认 90s,偏长:边缘/NAT 常在更早时
/// 悄悄关掉空闲连接,复用到死连接会报 "connection closed before message
/// completed"。取短一点,宁可多建几次连接。
pub const UPSTREAM_POOL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// 限额账号冷却缺省时长(秒):上游未给 `retry_after` 时使用。
pub const DEFAULT_COOLDOWN_SECS: u64 = 600;

/// 冷却时长下限(秒):防止上游异常值导致账号长时间不可用。
pub const MIN_COOLDOWN_SECS: u64 = 60;

/// 冷却时长上限(秒):封顶一天。
pub const MAX_COOLDOWN_SECS: u64 = 24 * 60 * 60;

/// JSON configuration file structure.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamBackend {
    #[default]
    Native,
    Node,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpstreamTransport {
    pub backend: UpstreamBackend,
    pub profile: String,
    pub node_path: String,
    /// Directory containing main.mjs and the locked undici dependency.
    pub transport_dir: String,
    pub startup_timeout_secs: u64,
}

impl Default for UpstreamTransport {
    fn default() -> Self {
        Self {
            backend: UpstreamBackend::Native,
            profile: "raycast-windows-2.13.1.0".into(),
            node_path: "node".into(),
            transport_dir: "transport/node".into(),
            startup_timeout_secs: 15,
        }
    }
}

/// JSON configuration file structure.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ConfigFile {
    pub upstream_transport: Option<UpstreamTransport>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub api_key: Option<String>,
    pub dashboard_password: Option<String>,
    pub dashboard_session_secret: Option<String>,
    pub raycast_signature_secret: Option<String>,
    /// SQLite 存储库路径,默认 `store/store.db`。
    pub accounts_db: Option<String>,
    /// 上游超时（秒），必须大于零，默认 20。
    pub upstream_timeout_secs: Option<std::num::NonZeroU64>,
    /// 上游代理链(可选):`http://`/`https://`/`socks5://` URL 列表。
    pub proxies: Option<Vec<String>>,
    /// 代理模式 `direct`(缺省) / `proxy` / `proxy_fallback_direct`,
    /// 语义与校验见 [`crate::proxies`]。
    #[serde(default, alias = "proxiesMode")]
    pub proxies_mode: Option<ProxyMode>,
    /// 发给上游的 `locale`(BCP47,如 `zh-CN` / `en-US`)。真客户端取运行
    /// 机器的系统区域;网关按代理出口地区配置以避免"locale 与 IP 地理
    /// 位置交叉异常"。缺省 `zh-CN`。
    #[serde(default, alias = "raycastLocale")]
    pub raycast_locale: Option<String>,
    /// `<user-preferences>` 系统块里的 `Timezone`(IANA 名,如
    /// `Asia/Shanghai` / `America/New_York`)。真客户端取运行机器时区;
    /// 网关应与代理出口地区(及 `raycast_locale`)一致。缺省 `Asia/Shanghai`。
    #[serde(default, alias = "raycastTimezone")]
    pub raycast_timezone: Option<String>,
}

/// Runtime configuration, loaded once at startup via [`Settings::from_path`].
#[derive(Clone, Debug)]
pub struct Settings {
    pub upstream_transport: UpstreamTransport,
    /// Signing secret for `X-Raycast-Signature-v2` (required, no built-in default).
    pub raycast_signature_secret: String,
    /// Required global gateway API key.
    pub api_key: String,
    /// Console login password. When absent, dashboard login is disabled.
    pub dashboard_password: Option<String>,
    /// Signs dashboard session cookies independently of the gateway API key.
    pub dashboard_session_secret: Option<String>,
    /// Primary address the HTTP server binds to (IPv4 or IPv6).
    pub bind_addr: SocketAddr,
    /// Dual-stack: secondary address for dual IPv4/IPv6 binding. `None` if single-stack.
    pub bind_addr_dual: Option<SocketAddr>,
    /// SQLite 账号库路径(账号表与后续模块共用)。
    pub accounts_db: String,
    /// 上游超时秒数(config.json 的同名键,缺省 20)。运行时统一通过
    /// [`Settings::upstream_timeout`] 取 `Duration`,不要直接换算。
    pub upstream_timeout_secs: u64,
    /// 上游代理链:按顺序尝试的代理 URL。
    pub proxies: Vec<String>,
    /// 代理模式;`proxy` / `proxy_fallback_direct` 保证 `proxies` 非空。
    pub proxies_mode: ProxyMode,
    /// chat 请求的 `locale`(BCP47)。真客户端发运行机器的系统区域
    /// (`navigator.language`);网关应与代理出口地区一致。
    /// `None` = 自动:direct 读本机,走代理经出口探测(`src/geo.rs`)。
    pub raycast_locale: Option<String>,
    /// `<user-preferences>` 系统块的 `Timezone`(IANA 名)。与
    /// `raycast_locale` 配套,共同声明"运行机器"的地理身份。
    /// `None` = 自动,语义同上。
    pub raycast_timezone: Option<String>,
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

    /// 上游超时(连接、响应头、首个 payload、流空闲、OAuth 共用这一个值)。
    pub fn upstream_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.upstream_timeout_secs)
    }

    /// Create Settings from a parsed ConfigFile.
    fn from_config_file(config: ConfigFile) -> Result<Self, String> {
        let raycast_signature_secret = non_empty(config.raycast_signature_secret)
            .ok_or_else(|| "missing raycast_signature_secret in config.json".to_string())?;
        let api_key = non_empty(config.api_key)
            .filter(|key| key.bytes().all(|b| b.is_ascii_graphic()))
            .ok_or_else(|| {
                "api_key in config.json is required and must use visible ASCII without whitespace"
                    .to_string()
            })?;

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

        let (proxies_mode, proxies) =
            crate::proxies::validate(config.proxies_mode, config.proxies)?;
        let upstream_transport = config.upstream_transport.unwrap_or_default();
        if upstream_transport.backend == UpstreamBackend::Node
            && (upstream_transport.profile != "raycast-windows-2.13.1.0"
                || upstream_transport.node_path.is_empty()
                || upstream_transport.transport_dir.is_empty()
                || upstream_transport.startup_timeout_secs == 0)
        {
            return Err("invalid upstream_transport: node requires supported profile, node_path, transport_dir and positive startup_timeout_secs".into());
        }
        let dashboard_password = non_empty(config.dashboard_password);
        let dashboard_session_secret = non_empty(config.dashboard_session_secret);
        // 显式值仍在配置层校验形态;空串视为未配置(走自动探测)。
        let raycast_locale = config
            .raycast_locale
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if let Some(locale) = &raycast_locale
            && !locale
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(format!(
                "raycast_locale must be a BCP47 tag like 'zh-CN' or 'en-US' (got {locale})"
            ));
        }
        let raycast_timezone = config
            .raycast_timezone
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if let Some(timezone) = &raycast_timezone
            && !timezone
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'/')
        {
            return Err(format!(
                "raycast_timezone must be an IANA name like 'Asia/Shanghai' or 'America/New_York' (got {timezone})"
            ));
        }
        if dashboard_password.is_some()
            && dashboard_session_secret
                .as_ref()
                .is_none_or(|secret| secret.len() < 32)
        {
            return Err("dashboard_session_secret must contain at least 32 bytes when dashboard_password is set".into());
        }

        Ok(Self {
            upstream_transport,
            raycast_signature_secret,
            api_key,
            dashboard_password,
            dashboard_session_secret,
            bind_addr,
            bind_addr_dual,
            upstream_timeout_secs: config
                .upstream_timeout_secs
                .map_or(20, std::num::NonZeroU64::get),
            accounts_db,
            proxies,
            proxies_mode,
            raycast_locale,
            raycast_timezone,
        })
    }
}

/// Treat an empty string in `config.json` the same as an absent key.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod proxy_tests {
    use super::*;

    #[test]
    fn upstream_backend_defaults_to_native_and_node_config_is_validated() {
        let parse = |extra: &str| {
            Settings::from_config_file(
                serde_json::from_str(&format!(
                    r#"{{"raycast_signature_secret":"s","api_key":"test-key"{extra}}}"#
                ))
                .unwrap(),
            )
        };
        assert_eq!(
            parse("").unwrap().upstream_transport.backend,
            UpstreamBackend::Native
        );
        assert_eq!(
            parse(r#", "upstream_transport":{"backend":"node"}"#)
                .unwrap()
                .upstream_transport
                .backend,
            UpstreamBackend::Node
        );
        for extra in [
            r#", "upstream_transport":{"backend":"node","profile":"unknown"}"#,
            r#", "upstream_transport":{"backend":"node","node_path":""}"#,
            r#", "upstream_transport":{"backend":"node","transport_dir":""}"#,
            r#", "upstream_transport":{"backend":"node","startup_timeout_secs":0}"#,
        ] {
            assert!(parse(extra).unwrap_err().contains("upstream_transport"));
        }
        assert!(
            serde_json::from_str::<ConfigFile>(r#"{"upstream_transport":{"backend":"other"}}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<ConfigFile>(r#"{"upstream_transport":{"nodePath":"node"}}"#)
                .is_err()
        );
    }

    #[test]
    fn dashboard_sessions_require_a_secret_with_at_least_32_bytes() {
        for secret in [
            None,
            Some("".into()),
            Some("x".repeat(31)),
            Some("x".repeat(32)),
        ] {
            let valid = secret.as_ref().is_some_and(|s: &String| s.len() >= 32);
            let result = Settings::from_config_file(ConfigFile {
                raycast_signature_secret: Some("secret".into()),
                api_key: Some("test-key".into()),
                dashboard_password: Some("password".into()),
                dashboard_session_secret: secret,
                ..Default::default()
            });
            assert_eq!(result.is_ok(), valid);
        }
    }

    #[test]
    fn dashboard_password_preserves_exact_value_and_disables_login_when_empty() {
        for (password, expected) in [
            (None, None),
            (Some(""), None),
            (Some(" 控制台 password "), Some(" 控制台 password ")),
        ] {
            let settings = Settings::from_config_file(ConfigFile {
                raycast_signature_secret: Some("secret".into()),
                api_key: Some("test-key".into()),
                dashboard_password: password.map(str::to_owned),
                dashboard_session_secret: Some("s".repeat(32)),
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
                r#"{"raycast_signature_secret":"s","api_key":"test-key"}"#,
                20,
            ),
            (
                r#"{"raycast_signature_secret":"s","api_key":"test-key","upstream_timeout_secs":45}"#,
                45,
            ),
        ] {
            let config = serde_json::from_str(json).unwrap();
            assert_eq!(
                Settings::from_config_file(config)
                    .unwrap()
                    .upstream_timeout()
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
    fn raycast_locale_defaults_to_zh_cn_and_validates_shape() {
        let parse = |extra: &str| {
            Settings::from_config_file(
                serde_json::from_str(&format!(
                    r#"{{"raycast_signature_secret":"s","api_key":"test-key"{extra}}}"#
                ))
                .unwrap(),
            )
        };
        // 缺省 zh-CN。
        assert_eq!(parse("").unwrap().raycast_locale, None);
        // camelCase 别名 + 自定义值。
        assert_eq!(
            parse(r#", "raycastLocale":"en-US""#).unwrap().raycast_locale,
            Some("en-US".to_string())
        );
        assert_eq!(
            parse(r#", "raycast_locale":"en-US""#).unwrap().raycast_locale,
            Some("en-US".to_string())
        );
        // 空白容忍、空串回落缺省。
        assert_eq!(
            parse(r#", "raycast_locale":" en-US ""#).unwrap().raycast_locale,
            Some("en-US".to_string())
        );
        assert_eq!(
            parse(r#", "raycast_locale":" ""#).unwrap().raycast_locale,
            None
        );
        // 非法字符拒绝。
        for bad in ["zh CN", "zh;CN"] {
            assert!(parse(&format!(r#", "raycast_locale":"{bad}""#)).is_err());
        }
        // timezone:缺省 Asia/Shanghai,camelCase 别名,空白容忍,非法拒绝。
        assert_eq!(parse("").unwrap().raycast_timezone, None);
        assert_eq!(
            parse(r#", "raycastTimezone":"America/New_York""#)
                .unwrap()
                .raycast_timezone,
            Some("America/New_York".to_string())
        );
        assert_eq!(
            parse(r#", "raycast_timezone":" Europe/Berlin ""#)
                .unwrap()
                .raycast_timezone,
            Some("Europe/Berlin".to_string())
        );
        assert_eq!(
            parse(r#", "raycast_timezone":" ""#).unwrap().raycast_timezone,
            None
        );
        for bad in ["Asia Shanghai", "Asia;Shanghai"] {
            assert!(parse(&format!(r#", "raycast_timezone":"{bad}""#)).is_err());
        }
    }

    #[test]
    fn leftover_model_overrides_key_is_ignored() {
        // 模型映射已改为控制台维护;旧 config.json 里没删的键不应让启动失败。
        let config = serde_json::from_str(
            r#"{"raycast_signature_secret":"s","api_key":"test-key","model_overrides":{"a":"b"},"modelOverrides":{"a":"b"}}"#,
        )
        .unwrap();
        assert!(Settings::from_config_file(config).is_ok());
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
    fn global_api_key_is_required_and_header_safe() {
        for api_key in [
            None,
            Some(""),
            Some(" "),
            Some("key with space"),
            Some("wrong\nkey"),
            Some("valid-key"),
        ] {
            let result = Settings::from_config_file(ConfigFile {
                api_key: api_key.map(str::to_owned),
                raycast_signature_secret: Some("secret".into()),
                ..Default::default()
            });
            if api_key == Some("valid-key") {
                assert_eq!(result.unwrap().api_key, "valid-key");
            } else {
                assert!(result.unwrap_err().contains("api_key"));
            }
        }
    }

    #[test]
    fn signature_secret_is_required() {
        for (json, missing) in [
            (r#"{"api_key":"test-key"}"#, "raycast_signature_secret"),
            (
                r#"{"raycast_signature_secret":""}"#,
                "raycast_signature_secret",
            ),
        ] {
            let error =
                Settings::from_config_file(serde_json::from_str(json).unwrap()).unwrap_err();
            assert!(error.contains(missing), "{json} -> {error}");
        }
    }

    #[test]
    fn proxies_mode_is_validated_against_proxies_at_startup() {
        let parse = |extra: &str| {
            Settings::from_config_file(
                serde_json::from_str(&format!(
                    r#"{{"raycast_signature_secret":"s","api_key":"test-key"{extra}}}"#
                ))
                .unwrap(),
            )
        };
        assert_eq!(parse("").unwrap().proxies_mode, ProxyMode::Direct);
        let ok =
            parse(r#","proxies_mode":"proxy_fallback_direct","proxies":["http://127.0.0.1:7897"]"#)
                .unwrap();
        assert_eq!(ok.proxies_mode, ProxyMode::ProxyFallbackDirect);
        assert!(
            parse(r#","proxies_mode":"proxy""#)
                .unwrap_err()
                .contains("proxies_mode")
        );
        // 旧布尔键已移除,值写错类型直接拒绝。
        assert!(serde_json::from_str::<ConfigFile>(r#"{"proxies_mode":true}"#).is_err());
    }
}
