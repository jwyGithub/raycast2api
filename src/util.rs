//! Small shared helpers with no external dependencies.

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
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15],
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
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15],
    )
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
    date_utc(unix_secs())
}

/// Format a Unix timestamp (whole seconds) as a UTC `YYYY-MM-DD` date.
fn date_utc(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert a count of days since the Unix epoch into a `(year, month, day)`
/// UTC civil date, via Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn fill_random(buf: &mut [u8]) -> std::io::Result<()> {
    std::fs::File::open("/dev/urandom")?.read_exact(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Only the private helpers are unit-tested here; the public id generators
    // are covered by `tests/id_derivation.rs`.

    #[test]
    fn date_utc_matches_known_dates() {
        assert_eq!(date_utc(0), "1970-01-01");
        assert_eq!(date_utc(946_684_800), "2000-01-01");
        assert_eq!(date_utc(1_609_459_200), "2021-01-01");
        assert_eq!(date_utc(1_756_944_000), "2025-09-04");
    }
}
