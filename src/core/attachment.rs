//! Inbound image attachment extraction shared by all three protocol adapters.
//!
//! Clients send images as either inline `data:` URLs (OpenAI `image_url`,
//! Responses `input_image`) or base64/url sources (Anthropic `image.source`).
//! Everything funnels into [`resolve_data_url`] / [`resolve_url_part`], which
//! produce canonical [`Attachment`]s with raw bytes. Remote `https://` URLs are
//! fetched with a bounded shared client — a failed fetch drops the attachment
//! (with a log line) rather than failing the request, mirroring how the real
//! client degrades images it cannot attach.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::core::message::Attachment;
use crate::util::unix_secs;

/// Resolve one inbound image reference (OpenAI/Anthropic/Responses all funnel
/// here): a `data:` URL decodes inline; an `https://` URL becomes a
/// [`Attachment::Remote`] placeholder the handler layer fetches; anything else
/// is unsupported and returns `None` (caller drops it).
pub fn from_url_reference(url: &str) -> Option<Attachment> {
    if let Some(attachment) = resolve_data_url(url) {
        return Some(attachment);
    }
    let url = url.trim();
    if url.starts_with("https://") {
        return Some(Attachment::Remote {
            url: url.to_string(),
        });
    }
    None
}

/// Media types the Raycast upstream accepts on vision-capable models
/// (`abilities.vision.formats`, reverse-engineered from the client bundle).
pub const VISION_FORMATS: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Hard cap for a single fetched remote image (16 MiB body limit already
/// bounds inline base64; remote fetches need their own bound).
const MAX_FETCH_BYTES: usize = 20 * 1024 * 1024;

/// Decode an inline `data:<mime>;base64,<payload>` URL into an attachment.
/// Returns `None` for non-data URLs (callers handle those separately) and for
/// undecodable payloads.
pub fn resolve_data_url(url: &str) -> Option<Attachment> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, payload) = rest.split_once(',')?;
    // The mimetype portion runs up to the ';' parameter separator; tolerate a
    // bare `data:image/png,` form too (non-base64 in-media payloads are
    // unsupported by every real client, so treat the rest as base64).
    let media_type = media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if !media_type.starts_with("image/") {
        return None;
    }
    let data = BASE64.decode(payload.trim()).ok()?;
    Some(Attachment::Inline { media_type, data })
}

/// Build an attachment from an Anthropic-style explicit base64 source.
pub fn from_base64(media_type: &str, data: &str) -> Option<Attachment> {
    if !media_type.starts_with("image/") {
        return None;
    }
    let data = BASE64.decode(data.trim()).ok()?;
    Some(Attachment::Inline {
        media_type: media_type.to_string(),
        data,
    })
}

/// Fetch a remote `https://` image URL. The shared client is process-wide
/// (connection reuse); failures return `None` so the caller can degrade.
pub async fn fetch_url(client: &reqwest::Client, url: &str) -> Option<Attachment> {
    if !url.starts_with("https://") {
        return None;
    }
    let started = std::time::Instant::now();
    let result = async {
        let response = client
            .get(url)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        let bytes = response.bytes().await?;
        Ok::<_, reqwest::Error>((content_type, bytes))
    }
    .await;
    match result {
        Ok((content_type, bytes)) if bytes.len() <= MAX_FETCH_BYTES => {
            // Sniff the type when the server sends a generic/missing one; the
            // magic-byte check doubles as validation that it is actually an image.
            let media_type = sniff(&content_type, &bytes)?;
            tracing::info!(
                bytes = bytes.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "fetched remote image attachment"
            );
            Some(Attachment::Inline {
                media_type,
                data: bytes.to_vec(),
            })
        }
        Ok((_, bytes)) => {
            tracing::warn!(
                bytes = bytes.len(),
                "remote image exceeds fetch cap, dropped"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                "remote image fetch failed, dropped: {}",
                crate::error::error_chain(&error)
            );
            None
        }
    }
}

/// Resolve the media type from a Content-Type header with byte-sniffing
/// fallback. `None` when the bytes are not a recognizable image.
fn sniff(declared: &str, bytes: &[u8]) -> Option<String> {
    if VISION_FORMATS.contains(&declared) {
        return Some(declared.to_string());
    }
    // PNG: 89 50 4E 47; JPEG: FF D8 FF; GIF: "GIF8"; WEBP: RIFF....WEBP.
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some("image/png".into());
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg".into());
    }
    if bytes.starts_with(b"GIF8") {
        return Some("image/gif".into());
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp".into());
    }
    None
}

/// A synthetic filename for the `ai/files` upload body. The real client sends
/// the attachment's display name; the gateway has none, so it derives one from
/// the media type. Deterministic per content hash + time so retries with the
/// same image reuse the cache key cleanly.
pub fn upload_filename(media_type: &str, data: &[u8]) -> String {
    use md5::Digest;
    let mut hasher = md5::Md5::new();
    hasher.update(data);
    hasher.update(unix_secs().to_be_bytes());
    let ext = media_type.strip_prefix("image/").unwrap_or("bin");
    format!("image-{}.{ext}", hex::encode(&hasher.finalize()[..4]))
}

/// The checksum field of the `ai/files` upload body: md5 of the raw bytes,
/// base64-encoded (Rails ActiveStorage convention, confirmed in the bundle).
pub fn upload_checksum(data: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD as B64;
    use md5::Digest;
    let mut hasher = md5::Md5::new();
    hasher.update(data);
    B64.encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1x1 PNG, base64 of the canonical 67-byte pixel.
    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    #[test]
    fn data_url_decodes_media_type_and_bytes() {
        let url = format!("data:image/png;base64,{PNG_B64}");
        let attachment = resolve_data_url(&url).unwrap();
        let Attachment::Inline { media_type, data } = attachment else {
            panic!("expected inline attachment");
        };
        assert_eq!(media_type, "image/png");
        assert!(data.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn data_url_rejects_non_image_and_bad_base64() {
        assert!(resolve_data_url("data:text/plain;base64,aGk=").is_none());
        assert!(resolve_data_url("data:image/png;base64,%%%").is_none());
        assert!(resolve_data_url("https://example.com/x.png").is_none());
    }

    #[test]
    fn url_reference_splits_inline_from_remote() {
        let inline = from_url_reference(&format!("data:image/png;base64,{PNG_B64}")).unwrap();
        assert!(matches!(inline, Attachment::Inline { .. }));
        let remote = from_url_reference("https://example.com/x.png").unwrap();
        assert!(matches!(remote, Attachment::Remote { .. }));
        // http:// (no TLS) is unsupported — the real client only ever
        // references https attachments.
        assert!(from_url_reference("http://example.com/x.png").is_none());
        assert!(from_url_reference("ftp://example.com/x.png").is_none());
    }

    #[test]
    fn explicit_base64_source_validates_media_type() {
        let a = from_base64("image/jpeg", PNG_B64).unwrap();
        assert!(matches!(&a, Attachment::Inline { media_type, .. } if media_type == "image/jpeg"));
        assert!(from_base64("application/pdf", PNG_B64).is_none());
    }

    #[test]
    fn sniff_recognizes_magic_bytes() {
        let png = BASE64.decode(PNG_B64).unwrap();
        assert_eq!(sniff("", &png).as_deref(), Some("image/png"));
        assert_eq!(
            sniff("application/octet-stream", &png).as_deref(),
            Some("image/png")
        );
        assert_eq!(
            sniff("binary", b"\xFF\xD8\xFFrest").as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(sniff("", b"GIF89a..."), Some("image/gif".to_string()));
        assert_eq!(sniff("", b"RIFF1234WEBP"), Some("image/webp".to_string()));
        assert_eq!(sniff("", b"plain text"), None);
    }

    #[test]
    fn upload_helpers_produce_stable_shapes() {
        let data = b"hello";
        // md5("hello") = 5d41402abc4b2a76b9719d911017c592 → base64
        assert_eq!(upload_checksum(data), "XUFAKrxLKna5cZ2REBfFkg==");
        let name = upload_filename("image/png", data);
        assert!(name.starts_with("image-") && name.ends_with(".png"));
    }
}
