//! Stream peeking: inspects the leading SSE lines of a 200 chat response so
//! the client can detect in-stream rejections (usage limits, unconfirmed
//! email) *before* handing the stream to the caller, rotate accounts, and
//! re-send.
//!
//! The peek must be invisible when there is no error: whatever bytes were read
//! are spliced back in front of the remaining stream via [`ByteStream`], and
//! the reconstructed [`reqwest::Response`] is byte-identical to what the
//! caller would have seen without peeking.
//!
//! Real Raycast streams do NOT start with the error payload directly — there
//! can be `event:`/comment keep-alive lines first. So the peek scans the
//! leading lines of the first chunk (stopping at the first `data:` line),
//! which is where a rejection shows up; content streams start with a `data:`
//! text delta instead and pass through untouched.

use futures_util::{Stream, StreamExt};

use super::wire::{RaycastSseError, RaycastSsePayload};

/// A peeked in-stream error: the parsed `error` object plus the raw payload
/// (needed to splice it back when the error is *not* handled here).
pub struct PeekedError {
    pub parsed: RaycastSseError,
    pub raw: String,
}

impl PeekedError {
    pub fn message(&self) -> String {
        self.parsed
            .message
            .clone()
            .unwrap_or_else(|| "upstream error (no message provided)".to_string())
    }
}

/// Peek the leading SSE lines of a 200 stream for an error payload. Scans
/// every line of the first chunk until (and including) the first `data:` line
/// — rejections arrive before any content — and returns `Some` when that
/// payload carries an `error` object. Without an error the response is rebuilt
/// with every byte back in place, byte-identical to not peeking at all.
pub async fn peek_first_payload(
    response: reqwest::Response,
    timeout: std::time::Duration,
) -> (Option<PeekedError>, reqwest::Response) {
    let status = response.status();
    let mut bytes = response.bytes_stream();
    let mut captured = Vec::new();
    let mut decoder = super::sse::SseDecoder::new();
    let mut error = None;
    let mut error_raw = String::new();
    let mut consumed = 0;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, bytes.next()).await {
            Ok(Some(Ok(chunk))) => {
                captured.extend_from_slice(&chunk);
                if captured.len() > 256 * 1024 {
                    break;
                }
                if let Some(payload) = decoder.push_bytes(&chunk).into_iter().next() {
                    if let Ok(parsed) = serde_json::from_str::<RaycastSsePayload>(&payload) {
                        error = parsed.error;
                        error_raw = payload;
                    }
                    // Consume only through the first data line; preserve subsequent payloads.
                    for line in captured.split_inclusive(|b| *b == b'\n') {
                        consumed += line.len();
                        if line.starts_with(b"data:") {
                            break;
                        }
                    }
                    break;
                }
            }
            Ok(Some(Err(error))) => {
                let replay =
                    futures_util::stream::iter([Ok(ByteChunk::from(captured)), Err(error)])
                        .chain(bytes);
                return (None, rebuild(status, replay, None));
            }
            Ok(None) => break,
            Err(_) => {
                let replay = futures_util::stream::iter([
                    Ok(ByteChunk::from(captured)),
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "upstream first payload timeout",
                    )),
                ]);
                let response = axum::http::Response::builder()
                    .status(status)
                    .body(reqwest::Body::wrap_stream(replay))
                    .unwrap();
                return (None, reqwest::Response::from(response));
            }
        }
    }
    if let Some(parsed) = error {
        // 是流内错误:消费已扫描字节(错误行含前面的事件行),剩余接回流前端。
        let rest = &captured[consumed..];
        let chunk = (!rest.is_empty()).then(|| ByteChunk::copy_from_slice(rest));
        return (
            Some(PeekedError {
                parsed,
                raw: error_raw,
            }),
            rebuild(status, bytes, chunk),
        );
    }

    // 不是错误流:整个 chunk 原样放回。
    (
        None,
        rebuild(status, bytes, Some(ByteChunk::from(captured))),
    )
}

/// Splice an error payload back to the front of a stream so the downstream
/// parser still sees it (as [`CanonicalEvent::Error`]) — used for in-stream
/// errors that are *not* handled by account rotation.
///
/// [`CanonicalEvent::Error`]: crate::core::event::CanonicalEvent::Error
pub fn restore_payload(response: reqwest::Response, raw: &str) -> reqwest::Response {
    let status = response.status();
    // 拼回完整 `data:` 行 —— 下游 SseDecoder 只认带前缀的 payload。
    let prefix = ByteChunk::from(format!("data: {raw}\n").into_bytes());
    rebuild(status, response.bytes_stream().boxed(), Some(prefix))
}

/// Rebuild a [`reqwest::Response`] over a replaced byte stream, keeping the
/// original status. The SSE content type is the only header downstream code
/// cares about.
fn rebuild<S>(
    status: reqwest::StatusCode,
    stream: S,
    prefix: Option<ByteChunk>,
) -> reqwest::Response
where
    S: Stream<Item = reqwest::Result<ByteChunk>> + Send + 'static,
{
    let stream: ByteStream = match prefix {
        Some(prefix) => ByteStream::prefixed(stream.boxed(), prefix),
        None => ByteStream::plain(stream.boxed()),
    };
    let http_response = axum::http::Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .body(reqwest::Body::wrap_stream(stream))
        .expect("valid http response");
    reqwest::Response::from(http_response)
}

/// One chunk of a response byte stream.
type ByteChunk = axum::body::Bytes;

/// A byte stream with an optional buffer spliced in front: after a peek, the
/// read bytes are put back so the downstream [`SseDecoder`] never notices.
///
/// [`SseDecoder`]: super::sse::SseDecoder
struct ByteStream {
    inner: futures_util::stream::BoxStream<'static, reqwest::Result<ByteChunk>>,
    /// 待放回的缓冲;`None` = 已放完,直接透传 inner。
    prefix: Option<ByteChunk>,
}

impl Stream for ByteStream {
    type Item = reqwest::Result<ByteChunk>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if let Some(prefix) = self.prefix.take() {
            return std::task::Poll::Ready(Some(Ok(prefix)));
        }
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl ByteStream {
    /// 把 `prefix` 拼在 `inner` 前面(空缓冲等价于 [`ByteStream::plain`])。
    fn prefixed(
        inner: futures_util::stream::BoxStream<'static, reqwest::Result<ByteChunk>>,
        prefix: impl Into<ByteChunk>,
    ) -> Self {
        let prefix: ByteChunk = prefix.into();
        Self {
            inner,
            prefix: (!prefix.is_empty()).then_some(prefix),
        }
    }

    /// 无前缀,直接透传。
    fn plain(inner: futures_util::stream::BoxStream<'static, reqwest::Result<ByteChunk>>) -> Self {
        Self {
            inner,
            prefix: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn configured_timeout_stops_waiting_for_first_payload() {
        let response = rebuild(
            reqwest::StatusCode::OK,
            futures_util::stream::pending::<reqwest::Result<ByteChunk>>(),
            None,
        );
        let (error, response) = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            peek_first_payload(response, std::time::Duration::from_millis(5)),
        )
        .await
        .unwrap();
        assert!(error.is_none());
        assert!(response.bytes().await.is_err());
    }

    #[tokio::test]
    async fn recognizes_error_across_chunks_after_keepalive() {
        let chunks = [
            b": ping\n".as_slice(),
            b"data: {\"error\":{\"message\":\"limit\",",
            b"\"http_status\":429}}\nrest\n",
        ];
        let response = rebuild(
            reqwest::StatusCode::OK,
            futures_util::stream::iter(chunks.map(|s| Ok(ByteChunk::from(s)))),
            None,
        );
        let (error, response) =
            peek_first_payload(response, std::time::Duration::from_secs(20)).await;
        assert_eq!(error.unwrap().parsed.http_status, 429);
        assert_eq!(drained(response).await, b"rest\n");
    }

    /// 用给定的首 chunk 构造一个待偷看的 200 响应。
    fn response_with(body: &'static [u8]) -> reqwest::Response {
        let stream = futures_util::stream::iter([Ok::<_, reqwest::Error>(ByteChunk::from(body))]);
        rebuild(reqwest::StatusCode::OK, stream, None)
    }

    /// 消费重建后的响应,拼回全部字节。
    async fn drained(response: reqwest::Response) -> Vec<u8> {
        use futures_util::StreamExt;
        let mut out = Vec::new();
        let mut bytes = response.bytes_stream();
        while let Some(Ok(chunk)) = bytes.next().await {
            out.extend_from_slice(&chunk);
        }
        out
    }

    #[tokio::test]
    async fn error_payload_is_peeked_and_rest_is_preserved() {
        let body = b"data: {\"error\":{\"message\":\"limit\",\"http_status\":429,\"retry_after\":447}}\ndata: {\"complete\":true}\n";
        let (peeked, rest) =
            peek_first_payload(response_with(body), std::time::Duration::from_secs(20)).await;
        let err = peeked.expect("error peeked");
        assert_eq!(err.parsed.http_status, 429);
        assert!(err.parsed.is_usage_limit());
        // 错误行被消费,剩余 payload 原样保留。
        assert_eq!(drained(rest).await, b"data: {\"complete\":true}\n");
    }

    /// 真实流形状:错误 payload 前有 event:/注释行(生产日志实测的失灵场景)。
    #[tokio::test]
    async fn error_after_event_lines_is_still_peeked() {
        let body = b": keep-alive\n\n: another comment\n\n: keep-alive\n\ndata: {\"error\":{\"message\":\"limit\",\"http_status\":429}}\ndata: {\"complete\":true}\n";
        let (peeked, rest) =
            peek_first_payload(response_with(body), std::time::Duration::from_secs(20)).await;
        let err = peeked.expect("error peeked through event lines");
        assert_eq!(err.parsed.http_status, 429);
        assert_eq!(drained(rest).await, b"data: {\"complete\":true}\n");
    }

    #[tokio::test]
    async fn normal_payload_is_passed_through_byte_identical() {
        let body = b"data: {\"text\":\"hi\"}\ndata: [DONE]\n";
        let (peeked, rest) =
            peek_first_payload(response_with(body), std::time::Duration::from_secs(20)).await;
        assert!(peeked.is_none());
        assert_eq!(drained(rest).await, body);
    }

    /// 内容流带 event: 行开头也应原样通过。
    #[tokio::test]
    async fn content_stream_with_event_prefix_passes_through() {
        let body = b"event: message\ndata: {\"text\":\"hi\"}\ndata: [DONE]\n";
        let (peeked, rest) =
            peek_first_payload(response_with(body), std::time::Duration::from_secs(20)).await;
        assert!(peeked.is_none());
        assert_eq!(drained(rest).await, body);
    }

    #[tokio::test]
    async fn chunk_without_complete_line_passes_through() {
        let body = b"data: {\"te";
        let (peeked, rest) =
            peek_first_payload(response_with(body), std::time::Duration::from_secs(20)).await;
        assert!(peeked.is_none());
        assert_eq!(drained(rest).await, body);
    }

    #[tokio::test]
    async fn restore_splices_raw_payload_back() {
        let body = b"data: {\"complete\":true}\n";
        let (peeked, rest) =
            peek_first_payload(response_with(body), std::time::Duration::from_secs(20)).await;
        assert!(peeked.is_none());

        let raw = r#"{"error":{"message":"limit","http_status":429}}"#;
        let restored = restore_payload(rest, raw);
        let expected: Vec<u8> = [format!("data: {raw}\n").as_bytes(), body.as_slice()].concat();
        assert_eq!(drained(restored).await, expected);
    }
}
