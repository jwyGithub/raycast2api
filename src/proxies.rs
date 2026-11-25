//! 上游代理策略:模式、启动期校验、通道顺序、代理客户端构造。
//!
//! 三种 `proxies_mode`:
//! * `direct`(缺省)—— 只走直连,失败即失败,永不使用代理;
//! * `proxy` —— 只走代理链(按序尝试),全部失败即失败,不回退直连;
//! * `proxy_fallback_direct` —— 先走代理链,全部失败再回退直连。
//!
//! 后两种必须配置非空 `proxies`,否则启动失败(宁可拒绝启动,也不静默直连泄露出口 IP)。

use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    #[default]
    Direct,
    Proxy,
    ProxyFallbackDirect,
}

impl ProxyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Proxy => "proxy",
            Self::ProxyFallbackDirect => "proxy_fallback_direct",
        }
    }
}

/// 启动期校验 `proxies_mode` + `proxies`:去空项、校验每个代理 URL、
/// 校验模式与代理列表是否匹配,并记录生效结果。
pub fn validate(
    mode: Option<ProxyMode>,
    proxies: Option<Vec<String>>,
) -> Result<(ProxyMode, Vec<String>), String> {
    let mode = mode.unwrap_or_default();
    let proxies = proxies
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
        reqwest::Proxy::all(p).map_err(|e| format!("invalid proxy '{p}': {e}"))?;
    }
    if mode != ProxyMode::Direct && proxies.is_empty() {
        return Err(format!(
            "proxies_mode '{}' requires a non-empty proxies list in config.json",
            mode.as_str()
        ));
    }
    let labels: Vec<_> = proxies.iter().map(|p| redact(p)).collect();
    if mode == ProxyMode::Direct && !proxies.is_empty() {
        tracing::warn!(
            mode = mode.as_str(),
            proxies = ?labels,
            "proxies are configured but ignored in direct mode"
        );
    } else {
        tracing::info!(mode = mode.as_str(), proxies = ?labels, "upstream proxy mode");
    }
    Ok((mode, proxies))
}

/// 真实客户端的两条出站 HTTP 管道(逆向 `backend/index.mjs` + 内嵌 Node 实测,
/// 2026-11-23;详见 doc/raycast-tls-fingerprint-2026-11-22.md §4.2)。
///
/// 放在本模块而非 `raycast::client`:两条管道的 **HTTP 版本差异(h2 vs
/// HTTP/1.1)是传输层/代理构建关注点**,`base_builder` 按它分叉 ALPN 行为;
/// UA/请求头语义只是管道的派生属性。避免低层 `proxies` 依赖高层 `raycast`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// AI 对话(`POST /ai/chat_completions`):内嵌 Node 的 undici `fetch`。
    /// UA 用 `x-Windows Version` 变体;undici 自动补 `accept-language: *`、
    /// `sec-fetch-mode: cors`、`accept-encoding: gzip, deflate`(Node
    /// 26.13.1 实测);
    /// ALPN 全量协商,服务端选 h2 即 h2。
    ChatFetch,
    /// 其余 API(got 实例,`prefixUrl .../api/v1`):/me、/ai/credits、
    /// models、`ai/files` 注册、oauth/token 等。UA 用 `x-Windows-amd64`
    /// 变体;got 自动补 `accept-encoding: gzip, deflate, br, zstd`,不补
    /// accept-language / sec-fetch-mode;真实客户端无 ALPN 扩展、纯
    /// HTTP/1.1,`http1_only()` 锁定。
    Got,
}

impl Transport {
    /// got/undici 自动写入的 `accept-encoding`。chat 值用 Raycast 自带的
    /// Node 26.13.1 二进制实测(`fetch()` 全局 undici → `gzip, deflate`,
    /// 2026-11-25);got 按 brotli+zstd 能力检测拼 `gzip, deflate, br, zstd`
    /// (index.mjs `vVt`/`yVt`,Node 26 两者皆可用)。
    pub fn accept_encoding(self) -> &'static str {
        match self {
            Self::ChatFetch => "gzip, deflate",
            Self::Got => "gzip, deflate, br, zstd",
        }
    }
}

/// 上游客户端的共享基础配置:连接/读取超时 + 连接池空闲回收。
///
/// 注意:代理模式下每个代理会按管道各建一个客户端(连接池 ×2),这是
/// 双管道对齐的固有成本,不是泄漏。
fn base_builder(timeout: std::time::Duration, transport: Transport) -> reqwest::ClientBuilder {
    let builder = Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .pool_idle_timeout(crate::config::UPSTREAM_POOL_IDLE_TIMEOUT);
    match transport {
        // native chat 协商 h2/http1; TLS 指纹及 ALPN 顺序由 Node 后端对齐。
        Transport::ChatFetch => builder,
        // native got 锁定 HTTP/1.1,仍发送 http/1.1 ALPN。
        Transport::Got => builder.http1_only(),
    }
}

/// got 管道原生直连客户端(HTTP/1.1-only,仍带 http/1.1 ALPN;
/// 完整 TLS 对齐由可选 Node 后端提供)。显式 `no_proxy`:直连必须是真直连,
/// 不能被 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 等环境变量悄悄接管。
pub fn direct_client(timeout: std::time::Duration) -> Client {
    base_builder(timeout, Transport::Got)
        .no_proxy()
        .build()
        .expect("failed to build HTTP client")
}

/// chat(undici fetch 管道)直连客户端:h2 能力版。
pub fn direct_chat_client(timeout: std::time::Duration) -> Client {
    base_builder(timeout, Transport::ChatFetch)
        .no_proxy()
        .build()
        .expect("failed to build HTTP chat client")
}

/// got 管道代理客户端(HTTP/1.1-only):每个代理一个共享客户端(连接池复用),
/// 与 `proxies` 一一对应;标签已去掉账号密码,可安全写入日志。构造失败视为
/// 配置错误,启动即崩。
pub fn build_clients(proxies: &[String], timeout: std::time::Duration) -> Vec<(String, Client)> {
    build_clients_for(proxies, timeout, Transport::Got)
}

/// [`build_clients`] 的 chat(h2 能力)变体。
pub fn build_chat_clients(
    proxies: &[String],
    timeout: std::time::Duration,
) -> Vec<(String, Client)> {
    build_clients_for(proxies, timeout, Transport::ChatFetch)
}

fn build_clients_for(
    proxies: &[String],
    timeout: std::time::Duration,
    transport: Transport,
) -> Vec<(String, Client)> {
    proxies
        .iter()
        .map(|url| {
            let proxy = reqwest::Proxy::all(url)
                .unwrap_or_else(|e| panic!("invalid proxy '{}': {e}", redact(url)));
            let client = base_builder(timeout, transport)
                .proxy(proxy)
                .build()
                .unwrap_or_else(|e| panic!("failed to build proxy client '{}': {e}", redact(url)));
            (redact(url), client)
        })
        .collect()
}

/// 按模式给出本次请求的通道顺序(标签,客户端);`direct` 模式永远不含代理。
pub fn channels<'a>(
    mode: ProxyMode,
    direct: &'a Client,
    proxies: &'a [(String, Client)],
) -> Vec<(&'a str, &'a Client)> {
    let direct = ("direct", direct);
    let proxies = proxies
        .iter()
        .map(|(label, client)| (label.as_str(), client));
    match mode {
        ProxyMode::Direct => vec![direct],
        ProxyMode::Proxy => proxies.collect(),
        ProxyMode::ProxyFallbackDirect => proxies.chain(std::iter::once(direct)).collect(),
    }
}

/// 去掉代理 URL 里的账号密码,避免凭据进入日志/请求记录。
fn redact(url: &str) -> String {
    reqwest::Url::parse(url).map_or_else(
        |_| "<invalid>".into(),
        |mut u| {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(list: &[&str]) -> Option<Vec<String>> {
        Some(list.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn mode_parses_snake_case_and_rejects_unknown() {
        for (text, mode) in [
            ("direct", ProxyMode::Direct),
            ("proxy", ProxyMode::Proxy),
            ("proxy_fallback_direct", ProxyMode::ProxyFallbackDirect),
        ] {
            let parsed: ProxyMode = serde_json::from_str(&format!("\"{text}\"")).unwrap();
            assert_eq!(parsed, mode);
            assert_eq!(parsed.as_str(), text);
        }
        assert!(serde_json::from_str::<ProxyMode>("\"true\"").is_err());
        assert!(serde_json::from_str::<ProxyMode>("true").is_err());
    }

    #[test]
    fn validate_defaults_to_direct_and_skips_empty_entries() {
        assert_eq!(validate(None, None).unwrap(), (ProxyMode::Direct, vec![]));
        assert_eq!(
            validate(None, urls(&[""])).unwrap(),
            (ProxyMode::Direct, vec![])
        );
        // direct 下代理被忽略但不报错。
        let (mode, proxies) =
            validate(Some(ProxyMode::Direct), urls(&["http://1.2.3.4:80"])).unwrap();
        assert_eq!((mode, proxies.len()), (ProxyMode::Direct, 1));
    }

    #[test]
    fn validate_checks_proxy_urls() {
        let ok = validate(
            Some(ProxyMode::Proxy),
            urls(&[
                "http://1.2.3.4:8080",
                "https://proxy.example.com",
                "socks5://u:p@127.0.0.1:1080",
            ]),
        )
        .unwrap();
        assert_eq!(ok.1.len(), 3);
        let err = validate(None, urls(&["ftp://1.2.3.4"])).unwrap_err();
        assert!(err.contains("invalid proxy"));
    }

    #[test]
    fn proxy_modes_require_proxies() {
        for mode in [ProxyMode::Proxy, ProxyMode::ProxyFallbackDirect] {
            for proxies in [None, urls(&[]), urls(&[""])] {
                let err = validate(Some(mode), proxies).unwrap_err();
                assert!(
                    err.contains(mode.as_str()) && err.contains("proxies"),
                    "{err}"
                );
            }
        }
    }

    #[test]
    fn channels_follow_mode() {
        let direct = Client::new();
        let proxies = vec![
            ("p1".to_string(), Client::new()),
            ("p2".to_string(), Client::new()),
        ];
        let labels = |mode| {
            channels(mode, &direct, &proxies)
                .into_iter()
                .map(|(label, _)| label)
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(ProxyMode::Direct), ["direct"]);
        assert_eq!(labels(ProxyMode::Proxy), ["p1", "p2"]);
        assert_eq!(
            labels(ProxyMode::ProxyFallbackDirect),
            ["p1", "p2", "direct"]
        );
    }

    #[test]
    fn labels_never_contain_credentials() {
        let clients = build_clients(
            &["http://user:secret@127.0.0.1:8080".to_string()],
            std::time::Duration::from_secs(1),
        );
        assert_eq!(clients[0].0, "http://127.0.0.1:8080/");
    }
}
