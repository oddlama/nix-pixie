//! Tests for `nix_pixie_proto::hex`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

#[test]
fn round_trip() {
    let bytes = [0u8, 1, 0xab, 0xff];
    assert_eq!(nix_pixie_proto::hex::encode(&bytes), "0001abff");
    assert_eq!(
        nix_pixie_proto::hex::decode("0001abff").as_deref(),
        Some(&bytes[..])
    );
    assert_eq!(
        nix_pixie_proto::hex::decode("0001ABFF").as_deref(),
        Some(&bytes[..])
    );
    assert_eq!(nix_pixie_proto::hex::decode("").as_deref(), Some(&[][..]));
}

#[test]
fn rejects_anything_but_digit_pairs() {
    for bad in ["abc", "zz", "é0", "+f", "-1", " f", "0x"] {
        assert_eq!(nix_pixie_proto::hex::decode(bad), None, "{bad:?}");
    }
}
