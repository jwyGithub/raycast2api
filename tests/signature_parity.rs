//! Fixed golden-vector parity tests for the `X-Raycast-Signature-v2` port.
//!
//! Verifies that the Rust implementation reproduces byte-identical values from
//! the reference TypeScript algorithm for a set of fixed inputs. The reference
//! values were produced by running the verbatim TS algorithm (the now-removed
//! `source/src/utils.ts`) under Node's WebCrypto.
//!
//! Unlike `signature_parity_har.rs`, which checks one end-to-end signature
//! against real captured Raycast traffic, this file guards the lower-level
//! `rot13_rot5_encode` transform and covers the default-secret, custom-secret,
//! and empty-body cases with stable golden values.

use raycast2api::signature::{calculate_signature_v2, rot13_rot5_encode};

// Fixed inputs shared across the signature checks.
const TS: &str = "2024-01-01T00:00:00.000Z";
const DEV: &str = "AB12-cd34-EF56-gh78";
const BODY: &str = r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}"#;

#[test]
fn rot13_rot5_encodes_letters_and_digits() {
    // ROT13 (letters) + ROT5 (digits); punctuation is untouched.
    assert_eq!(rot13_rot5_encode("AB12-cd34"), "NO67-pq89");
    assert_eq!(rot13_rot5_encode("Hello.World-0189"), "Uryyb.Jbeyq-5634");
    assert_eq!(
        rot13_rot5_encode("2024-01-01T00:00:00.000Z"),
        "7579-56-56G55:55:55.555M"
    );
}

#[test]
fn signature_matches_ts_reference_default_secret() {
    assert_eq!(
        calculate_signature_v2(TS, DEV, BODY, None),
        "5046124973472defc09a2a5c1e9185816d9ff2ae4d8800264fe9c377cf23fc01"
    );
}

#[test]
fn signature_matches_ts_reference_custom_secret() {
    assert_eq!(
        calculate_signature_v2(TS, DEV, BODY, Some("my-custom-secret")),
        "e7fc84c89b190051cd4abf9a6f791a90c4f9750c52bcf9c198f6d8f5ff4112a1"
    );
}

#[test]
fn signature_matches_ts_reference_empty_body() {
    assert_eq!(
        calculate_signature_v2(TS, DEV, "", None),
        "c36edbda8ff459fc968900deeb586a955a1bdead945542663a389766ca48022a"
    );
}
