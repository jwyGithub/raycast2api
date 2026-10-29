//! Stream peeking: inspects the first SSE payload of a 200 chat response so
//! the client can detect in-stream rejections (usage limits) *before* handing
//! the stream to the caller, rotate accounts, and re-send.
//!
//! The peek must be invisible when there is no error: whatever bytes were read
//! are spliced back in front of the remaining stream via [`ByteStream`], and
//! the reconstructed [`reqwest::Response`] is byte-identical to what the
//! caller would have seen without peeking.
//!
//! SSE payloads end with `\n`, and the first chunk of a fresh response almost
//! always contains the complete first payload, so only one chunk is ever read;
//! no cross-chunk buffering/waiting.

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

/// Peek the first `data:` payload of a 200 SSE stream. Returns `Some` only
/// when that payload carries an `error` object; otherwise the response is
/// rebuilt with every byte back in place and returned unchanged.
pub async fn peek_first_payload(
    response: reqwest::Response,
) -> (Option<PeekedError>, reqwest::Response) {
    let status = response.status();
    let mut bytes = response.bytes_stream();
    let Some(Ok(chunk)) = bytes.next().await else {
        // 流空或读错:交给上层按原样处理(与不偷看时行为一致)。
        return (None, rebuild(status, bytes.boxed(), None));
    };

    let Ok(text) = std::str::from_utf8(&chunk) else {
        return (None, rebuild(status, bytes, Some(chunk)));
    };

    // 找到首行(payload)及其后的剩余字节。
    let Some(nl) = text.find('\n') else {
        // 首 chunk 不足一个完整行:按无错误处理,剩余字节(含本行)原样还回。
        return (None, rebuild(status, bytes, Some(chunk)));
    };

    let (first_line, rest) = text.split_at(nl + 1);
    if let Some(data) = first_line.trim().strip_prefix("data:") {
        if let Some(error) = serde_json::from_str::<RaycastSsePayload>(data.trim())
            .ok()
            .and_then(|payload| payload.error)
        {
            // 是流内错误:消费首行,剩余字节(若有)接在流前端。
            let raw = data.trim().to_string();
            let err = PeekedError { parsed: error, raw };
            let chunk = (!rest.is_empty()).then(|| ByteChunk::from(rest.to_owned()));
            return (Some(err), rebuild(status, bytes, chunk));
        }
    }

    // 不是错误 payload:整个 chunk 原样放回。
    (None, rebuild(status, bytes, Some(chunk)))
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

    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
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
        Self { inner, prefix: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用给定的首 chunk 构造一个待偷看的 200 响应。
    fn response_with(body: &'static [u8]) -> reqwest::Response {
        let stream = futures_util::stream::iter([Ok::<_, reqwest::Error>(ByteChunk::from(body))]);
        rebuild(
            reqwest::StatusCode::OK,
            stream,
            None,
        )
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
        let (peeked, rest) = peek_first_payload(response_with(body)).await;
        let err = peeked.expect("error peeked");
        assert_eq!(err.parsed.http_status, 429);
        assert!(err.parsed.is_usage_limit());
        // 错误行被消费,剩余 payload 原样保留。
        assert_eq!(drained(rest).await, b"data: {\"complete\":true}\n");
    }

    #[tokio::test]
    async fn normal_payload_is_passed_through_byte_identical() {
        let body = b"data: {\"text\":\"hi\"}\ndata: [DONE]\n";
        let (peeked, rest) = peek_first_payload(response_with(body)).await;
        assert!(peeked.is_none());
        assert_eq!(drained(rest).await, body);
    }

    #[tokio::test]
    async fn chunk_without_complete_line_passes_through() {
        let body = b"data: {\"te";
        let (peeked, rest) = peek_first_payload(response_with(body)).await;
        assert!(peeked.is_none());
        assert_eq!(drained(rest).await, body);
    }

    #[tokio::test]
    async fn restore_splices_raw_payload_back() {
        let body = b"data: {\"complete\":true}\n";
        let (peeked, rest) = peek_first_payload(response_with(body)).await;
        assert!(peeked.is_none());

        let raw = r#"{"error":{"message":"limit","http_status":429}}"#;
        let restored = restore_payload(rest, raw);
        let expected: Vec<u8> = [format!("data: {raw}\n").as_bytes(), body.as_slice()].concat();
        assert_eq!(drained(restored).await, expected);
    }
}
