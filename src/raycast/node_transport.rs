//! Loopback framing adapter. Responses retain upstream status and streaming errors.
use super::node_process::Running;
use crate::{config::UpstreamTransport, error::GatewayError, proxies::Transport};
use futures_util::StreamExt;
use serde::Deserialize;
use std::sync::atomic::AtomicBool;
use tokio::sync::Mutex;

pub(crate) struct NodeTransport {
    pub(super) config: UpstreamTransport,
    pub(super) proxies: Vec<String>,
    pub(super) timeout: std::time::Duration,
    pub(super) token: String,
    pub(super) http: reqwest::Client,
    pub(super) running: Mutex<Option<Running>>,
    pub(super) stopping: AtomicBool,
}

#[derive(Deserialize)]
struct ResponseMetadata {
    version: u8,
    status: u16,
    headers: Vec<(String, String)>,
    decoded: bool,
}

impl NodeTransport {
    async fn endpoint(&self) -> Result<String, GatewayError> {
        if self.stopping.load(std::sync::atomic::Ordering::Acquire) {
            return Err(unavailable());
        }
        let mut state = self.running.lock().await;
        let running = state.as_mut().ok_or_else(unavailable)?;
        if running
            .child
            .try_wait()
            .map_err(|_| unavailable())?
            .is_some()
        {
            return Err(unavailable());
        }
        Ok(running.endpoint.clone())
    }

    pub(crate) async fn ready(&self) -> bool {
        let Ok(endpoint) = self.endpoint().await else {
            return false;
        };
        self.http
            .get(format!("{endpoint}/v1/health"))
            .bearer_auth(&self.token)
            .timeout(std::time::Duration::from_secs(1))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
    }

    pub(crate) async fn execute(
        &self,
        request: reqwest::Request,
        transport: Transport,
        channel: i32,
    ) -> Result<reqwest::Response, GatewayError> {
        let endpoint = self.endpoint().await?;
        let headers = request
            .headers()
            .iter()
            .map(|(name, value)| {
                value
                    .to_str()
                    .map(|value| (name.as_str(), value))
                    .map_err(|_| GatewayError::Internal("non-UTF8 upstream header".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let metadata = serde_json::json!({ "version": 1, "method": request.method().as_str(), "url": request.url().as_str(), "headers": headers,
            "channel": channel, "transport": match transport { Transport::ChatFetch => "chat", Transport::Got => "got" },
            "bodyLength": request.body().and_then(|body| body.as_bytes()).map_or(0, <[u8]>::len),
            "timeoutMs": request.timeout().copied().unwrap_or(self.timeout).as_millis() });
        let json = serde_json::to_vec(&metadata)?;
        if json.len() > 64 * 1024 {
            return Err(GatewayError::Internal(
                "Node request metadata exceeds 64 KiB".into(),
            ));
        }
        let mut body = Vec::with_capacity(json.len() + 4);
        body.extend_from_slice(&(json.len() as u32).to_be_bytes());
        body.extend_from_slice(&json);
        if let Some(request_body) = request.body() {
            let bytes = request_body.as_bytes().ok_or_else(|| {
                GatewayError::Internal("Node upstream body is not replayable".into())
            })?;
            body.extend_from_slice(bytes);
        }
        let mut ipc = self
            .http
            .post(format!("{endpoint}/v1/execute"))
            .bearer_auth(&self.token)
            .body(body);
        if let Some(timeout) = request.timeout() {
            ipc = ipc.timeout(*timeout);
        }
        let response = ipc.send().await.map_err(|error| {
            if error.is_connect() {
                unavailable()
            } else {
                GatewayError::from(error)
            }
        })?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            if status == 503 {
                return Err(unavailable());
            }
            // Error payloads contain sanitized stage/cause metadata only.
            let detail = response.text().await.unwrap_or_default();
            return Err(GatewayError::Upstream {
                status: 502,
                message: format!("Node upstream execution failed: {detail}"),
            });
        }
        let mut stream = response.bytes_stream();
        let mut captured = Vec::new();
        let length = loop {
            if captured.len() >= 4 {
                break u32::from_be_bytes(captured[..4].try_into().unwrap()) as usize;
            }
            let chunk = stream
                .next()
                .await
                .ok_or_else(|| invalid("truncated response frame"))??;
            captured.extend_from_slice(&chunk);
        };
        if length > 64 * 1024 {
            return Err(invalid("response metadata exceeds 64 KiB"));
        }
        while captured.len() < length + 4 {
            let chunk = stream
                .next()
                .await
                .ok_or_else(|| invalid("truncated response metadata"))??;
            captured.extend_from_slice(&chunk);
        }
        let metadata: ResponseMetadata = serde_json::from_slice(&captured[4..4 + length])
            .map_err(|_| invalid("invalid response metadata"))?;
        if metadata.version != 1 || !(200..=599).contains(&metadata.status) {
            return Err(invalid("invalid response status/version"));
        }
        let mut builder = axum::http::Response::builder().status(metadata.status);
        for (name, value) in metadata.headers {
            if metadata.decoded
                && matches!(
                    name.to_ascii_lowercase().as_str(),
                    "content-encoding" | "content-length"
                )
            {
                continue;
            }
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| invalid("invalid response header name"))?;
            let value = reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| invalid("invalid response header value"))?;
            builder = builder.header(name, value);
        }
        let prefix = captured.split_off(4 + length);
        let replay =
            futures_util::stream::once(async move { Ok::<_, reqwest::Error>(prefix.into()) })
                .chain(stream);
        Ok(reqwest::Response::from(
            builder
                .body(reqwest::Body::wrap_stream(replay))
                .map_err(|_| invalid("response reconstruction failed"))?,
        ))
    }
}

fn unavailable() -> GatewayError {
    GatewayError::Unavailable("Node upstream transport is unavailable".into())
}
fn invalid(message: &str) -> GatewayError {
    GatewayError::Upstream {
        status: 502,
        message: format!("Node IPC: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        response::IntoResponse,
        routing::{get, post},
    };
    use std::sync::Arc;

    fn config() -> UpstreamTransport {
        UpstreamTransport {
            backend: crate::config::UpstreamBackend::Node,
            node_path: std::env::var("RAYCAST_TEST_NODE_PATH")
                .expect("set RAYCAST_TEST_NODE_PATH to Node 26.13.1"),
            transport_dir: format!("{}/transport/node", env!("CARGO_MANIFEST_DIR")),
            ..Default::default()
        }
    }

    #[tokio::test]
    #[ignore = "requires Node 26.13.1 and transport/node npm ci"]
    async fn real_node_ipc_preserves_status_body_headers_and_partial_stream_errors() {
        let node = NodeTransport::new(config(), vec![], std::time::Duration::from_secs(3));
        node.start().await.unwrap();
        assert!(node.ready().await);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .route(
                "/status",
                get(|| async {
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "7")],
                        "limited",
                    )
                }),
            )
            .route("/echo", post(|body: axum::body::Bytes| async move { body }))
            .route(
                "/sse",
                get(|| async {
                    let stream = futures_util::stream::iter([Ok::<_, std::io::Error>(
                        axum::body::Bytes::from_static(b"data: partial\n\n"),
                    )])
                    .chain(futures_util::stream::once(async {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        Err(std::io::Error::other("upstream truncated"))
                    }));
                    (
                        [("content-type", "text/event-stream")],
                        axum::body::Body::from_stream(stream),
                    )
                        .into_response()
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        for transport in [Transport::ChatFetch, Transport::Got] {
            let response = node
                .execute(
                    http.get(format!("http://{address}/status"))
                        .build()
                        .unwrap(),
                    transport,
                    -1,
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 429);
            assert_eq!(response.headers()["retry-after"], "7");
            assert_eq!(response.text().await.unwrap(), "limited");
            let data = vec![0, 255, 13, 10, 128];
            let response = node
                .execute(
                    http.post(format!("http://{address}/echo"))
                        .body(data.clone())
                        .build()
                        .unwrap(),
                    transport,
                    -1,
                )
                .await
                .unwrap();
            assert_eq!(response.bytes().await.unwrap().as_ref(), data.as_slice());
            let response = node
                .execute(
                    http.get(format!("http://{address}/sse")).build().unwrap(),
                    transport,
                    -1,
                )
                .await
                .unwrap();
            let mut stream = response.bytes_stream();
            let mut partial = Vec::new();
            let mut error = false;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(chunk) => partial.extend_from_slice(&chunk),
                    Err(_) => {
                        error = true;
                        break;
                    }
                }
            }
            assert_eq!(partial, b"data: partial\n\n");
            assert!(error, "IPC must retain truncation as a body error");
        }
        node.shutdown().await;
        assert!(!node.ready().await);
        let result = node
            .execute(
                http.get(format!("http://{address}/status"))
                    .build()
                    .unwrap(),
                Transport::Got,
                -1,
            )
            .await;
        assert!(matches!(result, Err(GatewayError::Unavailable(_))));
        server.abort();
    }

    #[tokio::test]
    #[ignore = "requires Node 26.13.1 and transport/node npm ci"]
    async fn node_pool_selection_proxy_modes_and_oauth_single_attempt() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    post(move || {
                        let observed = observed.clone();
                        async move {
                            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            "ok"
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let state = crate::server::state::compact_test_state();
        let mut client = state.dispatcher.client.clone();
        Arc::make_mut(&mut client.settings).proxies_mode =
            crate::proxies::ProxyMode::ProxyFallbackDirect;
        let proxy = "http://127.0.0.1:1";
        client.node = Some(NodeTransport::new(
            config(),
            vec![proxy.into()],
            std::time::Duration::from_secs(3),
        ));
        client.proxies =
            crate::proxies::build_clients(&[proxy.into()], std::time::Duration::from_secs(3));
        client.chat_proxies =
            crate::proxies::build_chat_clients(&[proxy.into()], std::time::Duration::from_secs(3));
        client.start_upstream().await.unwrap();
        let url = format!("http://{address}/");
        let request = || client.http.post(&url).body("signed bytes").build().unwrap();
        // Fallback is chosen by Rust, after the configured proxy fails.
        let response = client.send_got(request(), None, None).await.unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        // PUT/OAuth policy only uses the first proxy, even in fallback mode.
        assert!(client.execute_got_once(request()).await.is_err());
        client.oauth_url = Some(url.clone());
        assert!(
            client
                .oauth_token(&serde_json::json!({"refresh_token":"test"}))
                .await
                .is_err()
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        Arc::make_mut(&mut client.settings).proxies_mode = crate::proxies::ProxyMode::Proxy;
        assert!(
            client
                .send_got(client.http.post(&url).build().unwrap(), None, None)
                .await
                .is_err()
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        client.shutdown_upstream().await;
        server.abort();
    }

    #[tokio::test]
    #[ignore = "requires Node 26.13.1 and transport/node npm ci"]
    async fn crashed_process_fails_fast_and_restarts_without_replaying_requests() {
        let node = NodeTransport::new(config(), vec![], std::time::Duration::from_secs(3));
        node.start().await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/",
                    get(move || {
                        let observed = observed.clone();
                        async move {
                            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            "ok"
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let request = || http.get(format!("http://{address}/")).build().unwrap();
        assert_eq!(
            node.execute(request(), Transport::Got, -1)
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "ok"
        );
        {
            let mut state = node.running.lock().await;
            let running = state.as_mut().unwrap();
            running.child.start_kill().unwrap();
            running.child.wait().await.unwrap();
        }
        assert!(!node.ready().await);
        assert!(matches!(
            node.execute(request(), Transport::Got, -1).await,
            Err(GatewayError::Unavailable(_))
        ));
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            while !node.ready().await {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "restart must not replay earlier requests"
        );
        assert_eq!(
            node.execute(request(), Transport::Got, -1)
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "ok"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        node.shutdown().await;
        assert!(node.start().await.is_err());
        assert!(!node.ready().await);
        server.abort();
    }
}
