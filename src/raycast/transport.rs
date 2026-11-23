//! Shared network channels and safe OAuth transport policy.
use super::client::RaycastClient;
use crate::{error::GatewayError, log::context as debug, proxies::ProxyMode};
impl RaycastClient {
    /// 所有端点共享通道顺序；仅网络错误换通道，HTTP 业务错误不重放。
    ///
    /// `transport` 同时决定用哪组 HTTP 客户端:chat 请求走 h2 能力组
    /// (对齐真实客户端的 undici fetch),其余端点走 HTTP/1.1 组(对齐 got)。
    pub(super) async fn send(
        &self,
        request: reqwest::Request,
        transport: super::client::Transport,
        context: Option<(&crate::statistics::CallContext, &str)>,
        mut reserved: Option<crate::statistics::Attempt>,
    ) -> Result<reqwest::Response, GatewayError> {
        let (direct, proxies) = match transport {
            super::client::Transport::ChatFetch => (&self.chat_http, &self.chat_proxies),
            super::client::Transport::Got => (&self.http, &self.proxies),
        };
        let channels = crate::proxies::channels(self.settings.proxies_mode, direct, proxies);
        let mut last = None;
        let body_bytes = request
            .body()
            .and_then(|body| body.as_bytes())
            .map(<[u8]>::len);
        let account = context.map(|(_, email)| email);
        let log = context.and_then(|(call, _)| {
            self.log
                .as_deref()
                .map(|log| (log, call.request.request_id.as_str()))
        });
        let mut request_logged = false;
        for (channel, client) in channels {
            let replay = request
                .try_clone()
                .ok_or_else(|| GatewayError::Internal("upstream body is not replayable".into()))?;
            let attempt = if let Some((call, email)) = context {
                if let Some(attempt) = reserved.take() {
                    attempt.bind_account(email)?;
                    Some(attempt)
                } else {
                    let attempt =
                        crate::statistics::Attempt::start(self.statistics.clone(), call, email);
                    if attempt.is_none() && call.accounting.as_ref().is_some_and(|key| key.required)
                    {
                        return Err(GatewayError::UsageTrackingUnavailable);
                    }
                    attempt
                }
            } else {
                None
            };
            if !request_logged && let Some((log, id)) = log {
                log.record_upstream_request(id, &replay);
                request_logged = true;
            }
            // 失败日志带耗时:瞬间失败(复用到死连接)与等几秒才失败(上游收下
            // 请求后主动断开)是两种不同成因,排查 "connection closed before
            // message completed" 时靠它区分。
            let started = std::time::Instant::now();
            match tokio::time::timeout(self.settings.upstream_timeout(), client.execute(replay))
                .await
            {
                Ok(Ok(mut response)) => {
                    if let Some((log, id)) = log {
                        log.record_response_headers(id, response.headers());
                    }
                    if let Some(attempt) = attempt {
                        attempt.set_http_status(response.status().as_u16());
                        response.extensions_mut().insert(attempt);
                    }
                    let id = debug::current_request_id();
                    if id != "-" {
                        debug::record_channel(&id, channel);
                    }
                    return Ok(response);
                }
                Ok(Err(error)) => {
                    tracing::warn!(
                        channel,
                        account,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        body_bytes,
                        is_connect = error.is_connect(),
                        "→ upstream send failed: {}",
                        crate::error::error_chain(&error)
                    );
                    last = Some(GatewayError::from(error));
                }
                Err(_) => {
                    tracing::warn!(
                        channel,
                        account,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        body_bytes,
                        "→ upstream send failed: response header timeout"
                    );
                    last = Some(GatewayError::Upstream {
                        status: 504,
                        message: "upstream response header timeout".into(),
                    });
                }
            }
            if let (Some(attempt), Some(error)) = (attempt, &last) {
                attempt.fail(error);
            }
        }
        Err(last.expect("at least one channel is always configured"))
    }

    pub(crate) async fn oauth_token(
        &self,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, GatewayError> {
        // OAuth 轮换不可重放:只用一个通道。direct 走直连,其余模式用首个代理。
        let http = match self.settings.proxies_mode {
            ProxyMode::Direct => &self.http,
            _ => self
                .proxies
                .first()
                .map_or(&self.http, |(_, client)| client),
        };
        let request = http
            .post(crate::config::OAUTH_TOKEN_URL)
            .json(body)
            .timeout(self.settings.upstream_timeout())
            .build()?;
        // OAuth rotation cannot safely replay after an ambiguous transport failure.
        Ok(http.execute(request).await?)
    }

    /// All non-chat endpoints: got pipeline (HTTP/1.1-only clients).
    pub(super) async fn send_got(
        &self,
        request: reqwest::Request,
        context: Option<(&crate::statistics::CallContext, &str)>,
        reserved: Option<crate::statistics::Attempt>,
    ) -> Result<reqwest::Response, GatewayError> {
        self.send(request, super::client::Transport::Got, context, reserved)
            .await
    }
}

#[cfg(test)]
mod tests {
    use crate::proxies::ProxyMode;
    use axum::{Router, routing::get};
    #[tokio::test]
    async fn statistics_record_each_network_channel_and_pending_header_cancellation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/", get(|| async { "ok" })).route(
                    "/pending",
                    get(|| async { std::future::pending::<&'static str>().await }),
                ),
            )
            .await
            .unwrap();
        });
        let state = crate::server::state::compact_test_state();
        let mut client = state.dispatcher.client.clone();
        std::sync::Arc::make_mut(&mut client.settings).proxies_mode =
            crate::proxies::ProxyMode::ProxyFallbackDirect;
        client.http = reqwest::Client::builder().no_proxy().build().unwrap();
        client.proxies = vec![(
            "unreachable".into(),
            reqwest::Client::builder()
                .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
                .build()
                .unwrap(),
        )];
        let canonical = crate::core::request::CanonicalRequest::new(
            crate::core::protocol::Protocol::Responses,
            "client",
        );
        let call =
            crate::statistics::CallContext::new(&canonical, "openai", "upstream", "inference");
        let request = client
            .http
            .get(format!("http://{address}/"))
            .build()
            .unwrap();
        let response = client
            .send(
                request,
                crate::raycast::client::Transport::Got,
                Some((&call, "a@example.com")),
                None,
            )
            .await
            .unwrap();
        assert_eq!(state.statistics.list(&Default::default()).unwrap().total, 2);
        drop(response);
        client.http = reqwest::Client::builder().no_proxy().build().unwrap();
        client.proxies.clear();
        let request = client
            .http
            .get(format!("http://{address}/pending"))
            .build()
            .unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(30),
                client.send(
                    request,
                    crate::raycast::client::Transport::Got,
                    Some((&call, "b@example.com")),
                    None
                )
            )
            .await
            .is_err()
        );
        let page = state.statistics.list(&Default::default()).unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(
            page.items
                .iter()
                .filter(|row| row.status == "failed")
                .count(),
            1
        );
        assert_eq!(
            page.items
                .iter()
                .filter(|row| row.status == "cancelled")
                .count(),
            2
        );
        assert!(page.items.iter().all(|row| row.usage_json.is_none()));
        server.abort();
    }
    #[tokio::test]
    async fn configured_timeout_limits_response_header_wait() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    get(|| async {
                        std::future::pending::<()>().await;
                        "unreachable"
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let state = crate::server::state::compact_test_state();
        let mut client = state.dispatcher.client.clone();
        std::sync::Arc::make_mut(&mut client.settings).upstream_timeout_secs = 1;
        client.http = reqwest::Client::builder().no_proxy().build().unwrap();
        let request = client
            .http
            .get(format!("http://{address}/"))
            .build()
            .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.send(request, crate::raycast::client::Transport::Got, None, None),
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            Err(crate::error::GatewayError::Upstream { status: 504, .. })
        ));
        server.abort();
    }

    #[tokio::test]
    async fn network_failure_rotates_channel_but_http_error_does_not() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    get(|| async { axum::http::StatusCode::TOO_MANY_REQUESTS }),
                ),
            )
            .await
            .unwrap();
        });
        let state = crate::server::state::compact_test_state();
        let mut client = state.dispatcher.client.clone();
        let direct_ok = || reqwest::Client::builder().no_proxy().build().unwrap();
        let dead = || {
            reqwest::Client::builder()
                .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
                .build()
                .unwrap()
        };
        let mode = |client: &mut crate::raycast::client::RaycastClient, mode| {
            std::sync::Arc::make_mut(&mut client.settings).proxies_mode = mode;
        };
        let get = |client: &crate::raycast::client::RaycastClient| {
            client
                .http
                .get(format!("http://{address}/"))
                .build()
                .unwrap()
        };
        // proxy_fallback_direct:代理网络失败 -> 回退直连;HTTP 429 不重放。
        mode(&mut client, ProxyMode::ProxyFallbackDirect);
        client.http = direct_ok();
        client.proxies = vec![("dead".into(), dead())];
        assert_eq!(
            client
                .send(get(&client), crate::raycast::client::Transport::Got, None, None)
                .await
                .unwrap()
                .status(),
            429
        );
        // proxy:代理全挂即失败,不回退直连(直连本身是通的)。
        mode(&mut client, ProxyMode::Proxy);
        assert!(
            client
                .send(get(&client), crate::raycast::client::Transport::Got, None, None)
                .await
                .is_err()
        );
        // proxy:第二个代理可用则换用成功。
        client.proxies = vec![("dead".into(), dead()), ("ok".into(), direct_ok())];
        assert_eq!(
            client
                .send(get(&client), crate::raycast::client::Transport::Got, None, None)
                .await
                .unwrap()
                .status(),
            429
        );
        // direct:直连失败即失败,不使用(可用的)代理。
        mode(&mut client, ProxyMode::Direct);
        client.http = dead();
        client.proxies = vec![("ok".into(), direct_ok())];
        let request = client
            .http
            .get(format!("http://{address}/"))
            .build()
            .unwrap();
        assert!(
            client
                .send(request, crate::raycast::client::Transport::Got, None, None)
                .await
                .is_err()
        );
        server.abort();
    }

    #[tokio::test]
    async fn chat_transport_selects_the_chat_pool_not_the_got_pool() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/", get(|| async { "ok" })),
            )
            .await
            .unwrap();
        });
        let state = crate::server::state::compact_test_state();
        let mut client = state.dispatcher.client.clone();
        // got 池指向死客户端:ChatFetch 若误用 got 池,请求必失败;
        // 正确选中 chat 池(loopback)则成功。
        let dead = || {
            reqwest::Client::builder()
                .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
                .build()
                .unwrap()
        };
        let ok = || reqwest::Client::builder().no_proxy().build().unwrap();
        client.http = dead();
        client.proxies = vec![];
        client.chat_http = ok();
        client.chat_proxies = vec![];
        let request = client
            .chat_http
            .get(format!("http://{address}/"))
            .build()
            .unwrap();
        let response = client
            .send(
                request,
                crate::raycast::client::Transport::ChatFetch,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        // 反向:Got 走死掉的 got 池,必须失败(证明两池确实分离)。
        let request = client
            .http
            .get(format!("http://{address}/"))
            .build()
            .unwrap();
        assert!(
            client
                .send(request, crate::raycast::client::Transport::Got, None, None)
                .await
                .is_err()
        );
        server.abort();
    }
}
