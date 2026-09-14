//! Parity test against real captured Raycast traffic.
//!
//! Verifies that [`calculate_signature_v2`] reproduces, byte-for-byte, the
//! `X-Raycast-Signature-v2` header that the real Raycast client sent for an
//! actual `/api/v1/ai/chat_completions` conversation request.
//!
//! The values below were extracted from the Charles capture
//! `charles/think_tools.har` (the smallest POST `chat_completions` entry). The
//! exact request body lives in `tests/fixtures/raycast_chat_min.json` so its
//! bytes are preserved verbatim — any whitespace change there would alter the
//! SHA-256 and break the signature, which is exactly what we want to guard.
//!
//! Because the capture used no custom secret, we sign with `None`, which also
//! confirms `DEFAULT_SIGNATURE_SECRET` is correct.

use raycast2api::signature::calculate_signature_v2;

/// `X-Raycast-Timestamp` from the captured request.
const TIMESTAMP: &str = "1788423597";
/// `X-Raycast-DeviceId` from the captured request.
const DEVICE_ID: &str = "69e4a49f117f3fc450970299a733d0bbe461a142e09fb54c9c6ded895f0e2f67";
/// `X-Raycast-Signature-v2` the real client sent for this exact body.
const EXPECTED_SIGNATURE: &str = "aa4827a927ceeb8bb9f36ff264d78b917e2ea945b4cd95542abb49a90ee3f0b2";

/// Exact request body captured from the wire (multi-byte UTF-8 characters
/// included). Embedded at compile time so the test is self-contained.
const BODY: &str = include_str!("fixtures/raycast_chat_min.json");

#[test]
fn reproduces_captured_conversation_signature() {
    let signature = calculate_signature_v2(TIMESTAMP, DEVICE_ID, BODY, None);

    assert_eq!(
        signature, EXPECTED_SIGNATURE,
        "signature for the captured conversation request did not match the \
         value Raycast actually sent (charles/think_tools.har)"
    );
}
