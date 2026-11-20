//! Tests for `nix_pixie::net`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::net::{dial_addrs, sanitise};
use std::net::SocketAddr;

#[test]
fn dial_addrs_prefers_routable_addresses() {
    let parse =
        |list: &[&str]| -> Vec<SocketAddr> { list.iter().map(|a| a.parse().unwrap()).collect() };
    let mixed = parse(&[
        "[fe80::1%3]:1",
        "192.168.1.2:1",
        "[2001:db8::2]:1",
        "169.254.7.7:1",
    ]);
    assert_eq!(
        dial_addrs(&mixed.into_iter().collect()),
        parse(&["169.254.7.7:1", "192.168.1.2:1", "[2001:db8::2]:1"])
    );
    let only_link_local = parse(&["[fe80::1%3]:1"]);
    assert_eq!(
        dial_addrs(&only_link_local.iter().copied().collect()),
        only_link_local
    );
}

#[test]
fn sanitise_strips_control_characters() {
    assert_eq!(sanitise("web\x1b[31m1\n\tok", 63), "web[31m1  ok");
    assert_eq!(sanitise("abcdef", 3), "abc");
}
