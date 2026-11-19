//! Small shared helpers.

use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix time in whole seconds (0 if the clock is before the epoch).
pub fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Generate a random v4-style UUID, without pulling in a UUID crate. Falls back
/// to a time-derived value if the OS RNG is unavailable.
pub fn new_uuid() -> String {
    let mut b = [0u8; 16];
    if fill_random(&mut b).is_err() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b.copy_from_slice(&nanos.to_le_bytes()); // u128 -> exactly 16 bytes
    }
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 1
    format_uuid(b)
}

/// Render 16 bytes in the canonical 8-4-4-4-12 lowercase-hex UUID layout.
fn format_uuid(b: [u8; 16]) -> String {
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// Crockford base32 alphabet used by ULIDs (excludes I, L, O, U).
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Generate a ULID (26-char Crockford base32): a 48-bit millisecond timestamp
/// followed by 80 bits of randomness. Falls back to a time-derived value if the
/// OS RNG is unavailable, mirroring [`new_uuid`].
pub fn new_ulid() -> String {
    let ms = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0) as u64)
        & 0xFFFF_FFFF_FFFF; // low 48 bits

    let mut rand = [0u8; 10];
    if fill_random(&mut rand).is_err() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        rand.copy_from_slice(&nanos.to_le_bytes()[..10]); // u128 -> first 10 bytes
    }

    // 48-bit timestamp in the high bits, 80-bit randomness in the low bits.
    let rand_bits = rand.iter().fold(0u128, |acc, &b| (acc << 8) | b as u128);
    encode_ulid(((ms as u128) << 80) | rand_bits)
}

/// 128-bit FNV-1a hash of `seed`. No external dependency; used only to derive
/// stable correlation/telemetry ids (`thread_id`, `message_id`), never as a
/// security token, so a non-cryptographic hash is deliberate.
fn fnv1a_128(seed: &[u8]) -> u128 {
    const FNV_OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const FNV_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut hash = FNV_OFFSET;
    for &b in seed {
        hash ^= b as u128;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Derive a **stable, deterministic** ULID-shaped id from an arbitrary seed:
/// the same bytes always map to the same 26-char id. Enables session affinity
/// for a stateless gateway — one conversation's `thread_id` can be re-derived
/// on every turn instead of minting a fresh random one.
pub fn derive_ulid(seed: &[u8]) -> String {
    encode_ulid(fnv1a_128(seed))
}

/// Derive a **stable, deterministic** UUID-shaped id from an arbitrary seed:
/// the same bytes always map to the same UUID. Used for `message_id`, which the
/// real client holds constant across the tool-call round-trips that resolve one
/// user turn (see the builder). Rendered as a well-formed RFC 9562 v8 (custom)
/// UUID — the bits come from [`fnv1a_128`], not a random source.
pub fn derive_uuid(seed: &[u8]) -> String {
    let mut b = fnv1a_128(seed).to_be_bytes();
    b[6] = (b[6] & 0x0f) | 0x80; // version 8 (custom)
    b[8] = (b[8] & 0x3f) | 0x80; // variant 1
    format_uuid(b)
}

/// Encode a 128-bit value as a 26-char Crockford base32 string (ULID width).
/// The field is 130 bits (26 * 5) with the top 2 bits always zero, so the
/// leading character is always in `0..=7`.
fn encode_ulid(value: u128) -> String {
    let mut out = [0u8; 26];
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = 125 - i * 5; // 125, 120, ..., 5, 0 (all non-negative)
        *slot = CROCKFORD[((value >> shift) & 0x1f) as usize];
    }
    out.iter().map(|&b| b as char).collect() // all bytes are ASCII
}

/// Today's date in UTC as `YYYY-MM-DD`, as the Raycast backend expects for
/// `current_date`.
pub fn current_date() -> String {
    time::OffsetDateTime::now_utc().date().to_string()
}

pub(crate) fn fill_random(buf: &mut [u8]) -> std::io::Result<()> {
    std::fs::File::open("/dev/urandom")?.read_exact(buf)
}

/// Mask an API key for display and storage: `****` between the first and last
/// few characters. Each side keeps at most `MASK_EDGE` characters and never more
/// than a quarter of the key, so at least half of it stays hidden; keys of 8
/// characters or fewer are hidden entirely. Single source for every `api_key_masked`.
pub fn mask_api_key(key: &str) -> String {
    const MASK_EDGE: usize = 8;
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "****".into();
    }
    let edge = (chars.len() / 4).min(MASK_EDGE);
    let head: String = chars[..edge].iter().collect();
    let tail: String = chars[chars.len() - edge..].iter().collect();
    format!("{head}****{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Only the private helpers are unit-tested here; the public id generators
    // are covered by `tests/id_derivation.rs`.

    #[test]
    fn current_date_is_iso_and_uuids_are_well_formed() {
        let date = current_date();
        assert_eq!(date.len(), 10);
        assert_eq!((&date[4..5], &date[7..8]), ("-", "-"));
        for id in [new_uuid(), derive_uuid(b"seed")] {
            assert_eq!(id.len(), 36);
            assert_eq!(
                id.match_indices('-').map(|(i, _)| i).collect::<Vec<_>>(),
                [8, 13, 18, 23]
            );
        }
    }

    #[test]
    fn mask_api_key_keeps_up_to_eight_chars_each_side() {
        // 生成的托管 Key（sk- + 64 位十六进制）：前后各 8 个字符。
        let generated = format!("sk-{}", "0123456789abcdef".repeat(4));
        assert_eq!(mask_api_key(&generated), "sk-01234****89abcdef");
        // 短 Key 每侧不超过四分之一，至少一半内容保持隐藏。
        assert_eq!(mask_api_key("abcdefghijklmnop"), "abcd****mnop");
        assert_eq!(mask_api_key("abcdefghi"), "ab****hi");
        // 8 个字符及以下不泄露任何内容；按字符而非字节切片，多字节字符不会 panic。
        assert_eq!(mask_api_key("abcdefgh"), "****");
        assert_eq!(mask_api_key(""), "****");
        assert_eq!(mask_api_key("密钥密钥密钥密钥密钥"), "密钥****密钥");
    }
}
