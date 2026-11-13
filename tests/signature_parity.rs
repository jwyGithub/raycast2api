//! Fixed golden-vector parity tests for the `X-Raycast-Signature-v2` port.
//!
//! Verifies that the Rust implementation reproduces byte-identical values from
//! the reference TypeScript algorithm for a set of fixed inputs. The reference
//! values were produced by running the verbatim TS algorithm (the now-removed
//! `source/src/utils.ts`) under Node's WebCrypto.
//!
//! This file guards the lower-level `rot13_rot5_encode` transform and covers
//! the custom-secret, empty-body and multi-byte UTF-8 body cases with stable
//! golden values.

use raycast2api::signature::{calculate_signature_v2, rot13_rot5_encode};

// Fixed inputs shared across the signature checks.
const TS: &str = "2024-01-01T00:00:00.000Z";
const DEV: &str = "AB12-cd34-EF56-gh78";
const BODY: &str = r#"{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}"#;
const SECRET: &str = "my-custom-secret";

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
fn signature_matches_ts_reference() {
    assert_eq!(
        calculate_signature_v2(TS, DEV, BODY, SECRET),
        "e7fc84c89b190051cd4abf9a6f791a90c4f9750c52bcf9c198f6d8f5ff4112a1"
    );
}

#[test]
fn signature_empty_body() {
    assert_eq!(
        calculate_signature_v2(TS, DEV, "", SECRET),
        "4258d0165c137ed0b902151948de8bca06be65a5475baa2f20a60da26a76651d"
    );
}

#[test]
fn signature_multibyte_utf8_body() {
    // The body is hashed as UTF-8 bytes, not as chars or UTF-16 code units.
    assert_eq!(
        calculate_signature_v2(TS, DEV, r#"{"text":"你好，世界 ✓"}"#, SECRET),
        "d8b46e533a1d4a5654135bc67ea93333a6b38bb42f9e5ab16f9e103f8f9a3007"
    );
}
