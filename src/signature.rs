//! Computation of the `X-Raycast-Signature-v2` header.
//!
//! Ported from the reference TypeScript implementation in
//! `source/src/utils.ts` (`calculateSignatureV2`). The algorithm is:
//!
//! 1. SHA-256 hash the request body, rendered as lowercase hex (`hash_hex`).
//! 2. Build `message = [timestamp, device_id, hash_hex]`, apply a
//!    ROT13 (letters) + ROT5 (digits) transform to each element, and join
//!    them with `.`.
//! 3. HMAC-SHA256 sign `message`, using the raw UTF-8 bytes of the secret
//!    *string* as the key (the secret is NOT hex-decoded), and return the
//!    signature as lowercase hex.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Default signing secret used when none is supplied, matching the fallback
/// in the reference implementation.
pub const DEFAULT_SIGNATURE_SECRET: &str =
    "6bc455473576ce2cd6f70426caff867aabbe3f7291c1a79681af5e8ce0ca1408";

/// Apply ROT13 to ASCII letters and ROT5 to ASCII digits; leave every other
/// character (and any non-ASCII character) unchanged.
///
/// This mirrors the JS reference, whose inputs (ISO timestamp, device id, and
/// a hex digest) are all ASCII. Exposed so the parity example
/// (`examples/signature_parity.rs`) can verify it directly.
pub fn rot13_rot5_encode(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'A'..='Z' => (((c as u8 - b'A' + 13) % 26) + b'A') as char,
            'a'..='z' => (((c as u8 - b'a' + 13) % 26) + b'a') as char,
            '0'..='9' => (((c as u8 - b'0' + 5) % 10) + b'0') as char,
            _ => c,
        })
        .collect()
}

/// Compute the `X-Raycast-Signature-v2` value.
///
/// * `timestamp`  - the value sent in `X-Raycast-Timestamp` (ISO-8601).
/// * `device_id`  - the value sent in `X-Raycast-DeviceId`.
/// * `body`       - the exact request body string that will be sent.
/// * `secret`     - the signing secret; pass `None` to use
///   [`DEFAULT_SIGNATURE_SECRET`].
pub fn calculate_signature_v2(
    timestamp: &str,
    device_id: &str,
    body: &str,
    secret: Option<&str>,
) -> String {
    let secret = secret.unwrap_or(DEFAULT_SIGNATURE_SECRET);

    // 1. SHA-256 of the body -> lowercase hex.
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    let hash_hex = hex::encode(hasher.finalize());

    // 2. ROT13+ROT5 each component, then join with '.'.
    let message = [timestamp, device_id, &hash_hex]
        .iter()
        .map(|part| rot13_rot5_encode(part))
        .collect::<Vec<_>>()
        .join(".");

    // 3. HMAC-SHA256 with the secret's raw UTF-8 bytes as the key.
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
