//! Offline tests for the public id generators in `util`.
//!
//! `new_ulid` mints a fresh ULID; `derive_ulid` / `derive_uuid` map a seed to a
//! *stable* id, which is what lets a stateless gateway reproduce the real
//! client's `thread_id` / `message_id` behaviour (see `tests/request_building.rs`
//! for the semantics built on top). Pure logic — no network involved.

use raycast2api::util::{derive_ulid, derive_uuid, new_ulid};

/// Crockford base32 alphabet (excludes I, L, O, U). Mirrors the private constant
/// in `util`; duplicated here so the assertions stay a black-box check.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[test]
fn ulid_is_26_crockford_chars() {
    let ulid = new_ulid();
    assert_eq!(ulid.len(), 26);
    assert!(
        ulid.bytes().all(|b| CROCKFORD.contains(&b)),
        "unexpected char in {ulid}"
    );
}

#[test]
fn ulids_are_unique() {
    assert_ne!(new_ulid(), new_ulid());
}

#[test]
fn derive_ulid_is_deterministic() {
    // Session affinity relies on this: same seed -> same thread_id.
    assert_eq!(
        derive_ulid(b"first user message"),
        derive_ulid(b"first user message")
    );
}

#[test]
fn derive_ulid_differs_by_seed() {
    assert_ne!(
        derive_ulid(b"conversation a"),
        derive_ulid(b"conversation b")
    );
}

#[test]
fn derive_ulid_is_26_crockford_chars() {
    let id = derive_ulid(b"seed");
    assert_eq!(id.len(), 26);
    assert!(
        id.bytes().all(|b| CROCKFORD.contains(&b)),
        "unexpected char in {id}"
    );
    // Top 2 bits of the 130-bit field are zero, so the leading char is 0..=7.
    assert!(
        b"01234567".contains(&id.as_bytes()[0]),
        "leading char out of range in {id}"
    );
}

#[test]
fn derive_uuid_is_deterministic() {
    // message_id stability within a user turn relies on this.
    assert_eq!(derive_uuid(b"turn prefix"), derive_uuid(b"turn prefix"));
}

#[test]
fn derive_uuid_differs_by_seed() {
    assert_ne!(derive_uuid(b"turn one"), derive_uuid(b"turn two"));
}

#[test]
fn derive_uuid_is_well_formed_v8() {
    let id = derive_uuid(b"seed");
    // 8-4-4-4-12 lowercase hex.
    let groups: Vec<&str> = id.split('-').collect();
    assert_eq!(
        groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
        vec![8, 4, 4, 4, 12]
    );
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'));
    // Version nibble 8, variant bits 0b10xx ('8'..='b').
    assert_eq!(id.as_bytes()[14], b'8', "version nibble must be 8 in {id}");
    assert!(
        b"89ab".contains(&id.as_bytes()[19]),
        "variant nibble out of range in {id}"
    );
}
