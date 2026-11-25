//! 按账号选择的代理传输与共享 HTTP 客户端构造。

pub mod store;

use reqwest::Client;

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
    /// `sec-fetch-mode: cors`、`accept-encoding: br, gzip, deflate, zstd`;
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
    /// got/undici 自动写入的 `accept-encoding`(实测,顺序即发包顺序)。
    pub fn accept_encoding(self) -> &'static str {
        match self {
            Self::ChatFetch => "br, gzip, deflate, zstd",
            Self::Got => "gzip, deflate, br, zstd",
        }
    }
}

/// 上游客户端的共享基础配置:连接/读取超时 + 连接池空闲回收。
///
/// 账号指定的代理按 got/chat 各缓存一个客户端，同一代理可被多个账号复用。
fn base_builder(timeout: std::time::Duration, transport: Transport) -> reqwest::ClientBuilder {
    let builder = Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .pool_idle_timeout(crate::config::UPSTREAM_POOL_IDLE_TIMEOUT);
    match transport {
        // chat:与 undici fetch 一致,ALPN 全量协商,服务端选 h2 即 h2。
        Transport::ChatFetch => builder,
        // got 系:真实客户端无 ALPN 扩展、纯 HTTP/1.1,锁定 http1。
        Transport::Got => builder.http1_only(),
    }
}

/// got 管道直连客户端(HTTP/1.1-only,ClientHello 无 ALPN 扩展,对齐真实
/// 客户端的 got 实例)。显式 `no_proxy`:直连必须是真直连,
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
