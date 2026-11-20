//! Tests for `nix_pixie_proto`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie_proto::*;

#[test]
fn alpns_carry_the_protocol_version() {
    let suffix = format!("/{PROTOCOL_VERSION}");
    for alpn in [ALPN_CTRL, ALPN_STORE, ALPN_LOG] {
        let alpn = std::str::from_utf8(alpn).unwrap();
        assert!(alpn.starts_with("nix-pixie/"), "{alpn}");
        assert!(alpn.ends_with(&suffix), "{alpn}");
    }
}
