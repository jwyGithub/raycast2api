//! Shared network channels and safe OAuth transport policy.
use super::client::RaycastClient;
use crate::{error::GatewayError, log::context as debug};
impl RaycastClient {
    /// 所有端点共享通道顺序；仅网络错误换通道，HTTP 业务错误不重放。
    pub(super) async fn send(
        &self,
        request: reqwest::Request,
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
        for (channel, client) in channels {
            let replay = request
                .try_clone()
                .ok_or_else(|| GatewayError::Internal("upstream body is not replayable".into()))?;
            match tokio::time::timeout(self.settings.upstream_timeout, client.execute(replay)).await
            {
                Ok(Ok(response)) => {
                    let id = debug::current_request_id();
                    if id != "-" && !self.proxies.is_empty() {
                        debug::record_channel(&id, channel);
                    }
                    return Ok(response);
                }
                Ok(Err(error)) => {
                    last = Some(GatewayError::from(error));
                }
                Err(_) => {
                    last = Some(GatewayError::Upstream {
                        status: 504,
                        message: "upstream response header timeout".into(),
                    });
                }
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
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), client.send(request))
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
        assert_eq!(client.send(request).await.unwrap().status(), 429);
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
        assert_eq!(client.send(request).await.unwrap().status(), 429);
        server.abort();
    }
}
