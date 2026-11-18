//! Shared network channels and safe OAuth transport policy.
use super::client::RaycastClient;
use crate::{error::GatewayError, log::context as debug};
impl RaycastClient {
    /// 所有端点共享通道顺序；仅网络错误换通道，HTTP 业务错误不重放。
    pub(super) async fn send(
        &self,
        request: reqwest::Request,
        context: Option<(&crate::statistics::CallContext, &str)>,
    ) -> Result<reqwest::Response, GatewayError> {
        let direct = std::iter::once(("direct", &self.http));
        let proxies = self
            .proxies
            .iter()
            .map(|(url, client)| (url.as_str(), client));
        let channels: Vec<_> = if self.settings.proxies_enabled {
            proxies.chain(direct).collect()
        } else {
            direct.chain(proxies).collect()
        };
        let mut last = None;
        let body_bytes = request
            .body()
            .and_then(|body| body.as_bytes())
            .map(<[u8]>::len);
        let account = context.map(|(_, email)| email);
        for (channel, client) in channels {
            let replay = request
                .try_clone()
                .ok_or_else(|| GatewayError::Internal("upstream body is not replayable".into()))?;
            let attempt = context.and_then(|(call, email)| {
                crate::statistics::Attempt::start(self.statistics.clone(), call, email)
            });
            // 失败日志带耗时:瞬间失败(复用到死连接)与等几秒才失败(上游收下
            // 请求后主动断开)是两种不同成因,排查 "connection closed before
            // message completed" 时靠它区分。
            let started = std::time::Instant::now();
            match tokio::time::timeout(self.settings.upstream_timeout, client.execute(replay)).await
            {
                Ok(Ok(mut response)) => {
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
        Err(last.expect("direct channel always exists"))
    }

    pub(crate) async fn oauth_token(
        &self,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, GatewayError> {
        let http = if self.settings.proxies_enabled {
            self.proxies
                .first()
                .map(|(_, client)| client)
                .unwrap_or(&self.http)
        } else {
            &self.http
        };
        let request = http
            .post(crate::config::OAUTH_TOKEN_URL)
            .json(body)
            .timeout(self.settings.upstream_timeout)
            .build()?;
        // OAuth rotation cannot safely replay after an ambiguous transport failure.
        Ok(http.execute(request).await?)
    }
}

#[cfg(test)]
mod tests {
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
        let mut client = state.client.clone();
        client.http = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
            .build()
            .unwrap();
        client.proxies = vec![(
            "fallback".into(),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        )];
        let canonical = crate::core::request::CanonicalRequest::new(
            crate::core::protocol::Protocol::Responses,
            "client",
        );
        let upstream =
            crate::raycast::builder::build_chat_request(&canonical, "openai", "upstream");
        let call = crate::statistics::CallContext::new(&canonical, &upstream, "inference");
        let request = client
            .http
            .get(format!("http://{address}/"))
            .build()
            .unwrap();
        let response = client
            .send(request, Some((&call, "a@example.com")))
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
                client.send(request, Some((&call, "b@example.com")))
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
        let mut client = state.client.clone();
        std::sync::Arc::make_mut(&mut client.settings).upstream_timeout =
            std::time::Duration::from_millis(5);
        client.http = reqwest::Client::builder().no_proxy().build().unwrap();
        let request = client
            .http
            .get(format!("http://{address}/"))
            .build()
            .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client.send(request, None),
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
        let mut client = state.client.clone();
        client.http = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
            .build()
            .unwrap();
        client.proxies = vec![(
            "test-fallback".into(),
            reqwest::Client::builder().no_proxy().build().unwrap(),
        )];
        let request = client
            .http
            .get(format!("http://{address}/"))
            .build()
            .unwrap();
        assert_eq!(client.send(request, None).await.unwrap().status(), 429);
        client.http = reqwest::Client::builder().no_proxy().build().unwrap();
        client.proxies = vec![(
            "unreachable".into(),
            reqwest::Client::builder()
                .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
                .build()
                .unwrap(),
        )];
        let request = client
            .http
            .get(format!("http://{address}/"))
            .build()
            .unwrap();
        assert_eq!(client.send(request, None).await.unwrap().status(), 429);
        server.abort();
    }
}
