//! Tests for `nix_pixie_proto::rendezvous`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie_proto::PROTOCOL_VERSION;
use nix_pixie_proto::announce::{AnnounceError, Announcement, MAX_LABEL, Pairing};
use nix_pixie_proto::rendezvous::*;

fn announcement(nameplate: u32) -> Announcement {
    Announcement::new([7; 32], 47011, Pairing::ShowsCode { nameplate }, "target")
}

/// Both sides must derive the same key from the nameplate alone, in every
/// version: changing this strands targets shown by older installer images.
#[test]
fn secret_is_stable() {
    assert_eq!(
        nix_pixie_proto::hex::encode(&secret(482_913)),
        "ae7a3040328adf0fc7236895c6a410bde982b41503b05c7603a4ef1a931f58a5"
    );
}

#[test]
fn secrets_differ_per_nameplate() {
    assert_ne!(secret(100_000), secret(100_001));
    assert_ne!(secret(1), secret(256));
}

#[test]
fn round_trip() {
    let announcement = announcement(482_913);
    let reply = encode(&announcement).unwrap();
    assert_eq!(decode(&reply), Ok(announcement));
}

/// The biggest announcement there is still fits.
#[test]
fn replies_fit() {
    let label = "\u{10348}".repeat(100);
    let announcement = Announcement::new(
        [0xff; 32],
        u16::MAX,
        Pairing::ShowsCode { nameplate: 999_999 },
        &label,
    );
    assert_eq!(announcement.label.chars().count(), MAX_LABEL);
    assert!(encode(&announcement).unwrap().len() <= MAX_REPLY);
}

#[test]
fn decode_rejects_what_it_should() {
    assert_eq!(decode(&[]), Err(AnnounceError::Invalid("reply")));
    assert_eq!(decode(&[0xff; 16]), Err(AnnounceError::Invalid("reply")));
    assert_eq!(
        decode(&vec![0; MAX_REPLY + 1]),
        Err(AnnounceError::Invalid("reply"))
    );

    let mut other_version = announcement(482_913);
    other_version.version = PROTOCOL_VERSION + 1;
    assert_eq!(
        decode(&postcard::to_stdvec(&other_version).unwrap()),
        Err(AnnounceError::Version(PROTOCOL_VERSION + 1))
    );

    // A nameplate outside both spaces.
    let invalid = announcement(100);
    assert_eq!(
        decode(&postcard::to_stdvec(&invalid).unwrap()),
        Err(AnnounceError::Invalid("np"))
    );
}
