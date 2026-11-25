//! 按已选账号的出口发送请求，并处理账号配置的直连回退与 OAuth 续期。
use super::client::RaycastClient;
use crate::{error::GatewayError, log::context as debug};
impl RaycastClient {
    /// 所有账号端点复用该账号的出口；仅启用回退时允许代理失败后直连。
    /// 网络错误、响应头超时和代理认证失败可回退，HTTP 业务错误不重放。
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
        let channels = self.request_channels(transport)?;
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
                    // Proxy authentication failure is a channel failure, unlike account 401/429.
                    if self.fallback_direct()
                        && channel != "direct"
                        && response.status() == reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED
                    {
                        let error = GatewayError::Upstream {
                            status: 407,
                            message: "proxy authentication failed".into(),
                        };
                        if let Some(attempt) = attempt {
                            attempt.set_http_status(407);
                            attempt.fail(&error);
                        }
                        last = Some(error);
                        continue;
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
        // OAuth 按账号选择直连或其指定代理；不重放可能已完成令牌轮换的请求。
        let channels = self.request_channels(super::client::Transport::Got)?;
        let (_, http) = channels[0];
        let request = http
            .post(crate::config::OAUTH_TOKEN_URL)
            .json(body)
            .timeout(self.settings.upstream_timeout())
            .build()?;
        // OAuth rotates tokens: only connection establishment failure is safe to replay.
        match http.execute(request).await {
            Err(error) if self.fallback_direct() && error.is_connect() => {
                let direct_request = self
                    .http
                    .post(crate::config::OAUTH_TOKEN_URL)
                    .json(body)
                    .timeout(self.settings.upstream_timeout())
                    .build()?;
                Ok(self.http.execute(direct_request).await?)
            }
            result => Ok(result?),
        }
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
    use crate::raycast::client::{AccountProxy, AccountRoute};
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
        client.http = reqwest::Client::builder().no_proxy().build().unwrap();
        client.account_route = AccountRoute::Proxy(AccountProxy {
            id: "unreachable".into(),
            fallback_direct: true,
            http: reqwest::Client::builder()
                .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
                .build()
                .unwrap(),
            chat_http: client.chat_http.clone(),
        });
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
        client.account_route = AccountRoute::Direct;
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
        client.account_route = AccountRoute::Direct;
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
    async fn account_proxy_fallback_handles_network_failure_without_replaying_http_errors() {
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
        let proxy = |http: reqwest::Client, fallback_direct| {
            AccountRoute::Proxy(AccountProxy {
                id: "selected-proxy".into(),
                fallback_direct,
                chat_http: http.clone(),
                http,
            })
        };
        let get = |client: &crate::raycast::client::RaycastClient| {
            client
                .http
                .get(format!("http://{address}/"))
                .build()
                .unwrap()
        };
        // 账号勾选回退：其指定代理连接失败后直连；HTTP 429 不重放。
        client.account_route = proxy(dead(), true);
        client.http = direct_ok();
        assert_eq!(
            client
                .send(
                    get(&client),
                    crate::raycast::client::Transport::Got,
                    None,
                    None
                )
                .await
                .unwrap()
                .status(),
            429
        );
        // 账号未勾选回退：指定代理失败即报错，即使直连可达。
        client.account_route = proxy(dead(), false);
        assert!(
            client
                .send(
                    get(&client),
                    crate::raycast::client::Transport::Got,
                    None,
                    None
                )
                .await
                .is_err()
        );
        // 指定代理返回 HTTP 429：原样返回，不尝试直连。
        client.http = dead();
        client.account_route = proxy(direct_ok(), true);
        assert_eq!(
            client
                .send(
                    get(&client),
                    crate::raycast::client::Transport::Got,
                    None,
                    None
                )
                .await
                .unwrap()
                .status(),
            429
        );
        // 账号选择直连：直连失败即报错，不使用任何代理。
        client.account_route = AccountRoute::Direct;
        client.http = dead();
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
            axum::serve(listener, Router::new().route("/", get(|| async { "ok" })))
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
        client.account_route = AccountRoute::Direct;
        client.chat_http = ok();
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
