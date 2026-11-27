//! Tests for `nix_pixie_proto::announce`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie_proto::announce::*;
use std::collections::HashMap;

fn shows(nameplate: u32) -> Announcement {
    Announcement::new([7; 32], 47011, Pairing::ShowsCode { nameplate }, "target")
}

fn from_map(map: &HashMap<&'static str, String>) -> Result<Announcement, AnnounceError> {
    Announcement::from_txt(|k| map.get(k).map(String::as_str), 47011)
}

#[test]
fn txt_round_trip() {
    let typed = Announcement::new([9; 32], 47011, Pairing::TypedControllerCode, "x");
    for announcement in [shows(42), typed] {
        let map: HashMap<_, _> = announcement.txt().into_iter().collect();
        assert_eq!(from_map(&map), Ok(announcement));
    }
}

#[test]
fn txt_rejects_bad_records() {
    let good: HashMap<_, _> = shows(42).txt().into_iter().collect();
    let with = |key, value: &str| {
        let mut map = good.clone();
        map.insert(key, value.to_string());
        from_map(&map)
    };
    // An installer older or newer than the controller.
    assert_eq!(with("v", "1"), Err(AnnounceError::Version(1)));
    assert_eq!(with("v", "3"), Err(AnnounceError::Version(3)));
    assert_eq!(with("np", "0"), Err(AnnounceError::Invalid("np")));
    assert_eq!(with("np", "100"), Err(AnnounceError::Invalid("np")));
    assert_eq!(with("id", "abcd"), Err(AnnounceError::Invalid("id")));
    assert_eq!(with("pair", "maybe"), Err(AnnounceError::Invalid("pair")));
    assert_eq!(
        with("label", &"x".repeat(64)),
        Err(AnnounceError::Invalid("label"))
    );
    let mut missing = good.clone();
    missing.remove("id");
    assert_eq!(from_map(&missing), Err(AnnounceError::Missing("id")));
}

#[test]
fn probe_round_trip_and_rejects_garbage() {
    let nonce = [5; 16];
    let probe = encode_probe(&nonce);
    assert_eq!(probe.len(), PROBE_LEN);
    assert_eq!(decode_probe(&probe), Some(nonce));
    assert_eq!(decode_probe(&probe[..PROBE_LEN - 1]), None);
    let mut wrong_magic = probe.clone();
    wrong_magic[0] ^= 1;
    assert_eq!(decode_probe(&wrong_magic), None);
}

#[test]
fn reply_round_trip_checks_nonce() {
    let nonce = [5; 16];
    let reply = encode_reply(&nonce, &shows(42)).unwrap();
    assert_eq!(decode_reply(&reply, &nonce), Ok(shows(42)));
    assert_eq!(
        decode_reply(&reply, &[6; 16]),
        Err(AnnounceError::Invalid("nonce"))
    );
    assert_eq!(
        decode_reply(&reply[..20], &nonce),
        Err(AnnounceError::Invalid("reply"))
    );
    assert!(decode_reply(&encode_probe(&nonce), &nonce).is_err());
}

/// The biggest possible reply (63 four-byte characters) still fits in a
/// probe, so targets never send more than they receive.
#[test]
fn replies_are_smaller_than_probes() {
    let label = "\u{10348}".repeat(100);
    let announcement = Announcement::new(
        [0xff; 32],
        u16::MAX,
        Pairing::ShowsCode { nameplate: 999_999 },
        &label,
    );
    assert_eq!(announcement.label.chars().count(), MAX_LABEL);
    assert!(encode_reply(&[0; 16], &announcement).unwrap().len() < PROBE_LEN);
}
