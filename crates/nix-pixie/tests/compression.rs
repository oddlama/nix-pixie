//! Tests for `nix_pixie::push`'s compression: by the connection's path.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie::net::Route;
use nix_pixie::push::{Compression, compresses};

fn direct(addr: &str) -> Route {
    Route::Direct(addr.parse().unwrap())
}

#[test]
fn nearby_paths_go_uncompressed_and_far_ones_compressed() {
    for nearby in [
        "[fe80::2%5]:47011",
        "192.168.1.20:47011",
        "10.0.3.195:47011",
        "169.254.7.7:47011",
        "[fd12:3456::1]:47011",
    ] {
        assert!(
            !compresses(Compression::Auto, &direct(nearby), &[]),
            "{nearby}"
        );
    }
    assert!(compresses(
        Compression::Auto,
        &direct("203.0.113.7:47011"),
        &[]
    ));
    assert!(compresses(
        Compression::Auto,
        &direct("[2001:db8::7]:47011"),
        &[]
    ));
    let relayed = Route::Relayed("https://relay.example.org".parse().unwrap());
    assert!(compresses(Compression::Auto, &relayed, &[]));
    assert!(compresses(Compression::Auto, &Route::Unknown, &[]));
}

#[test]
fn compression_can_be_forced_either_way() {
    let lan = direct("192.168.1.20:47011");
    let relayed = Route::Relayed("https://relay.example.org".parse().unwrap());
    assert!(compresses(Compression::Zstd, &lan, &[]));
    assert!(!compresses(Compression::None, &relayed, &[]));
}

#[test]
fn hosts_on_the_controllers_networks_are_nearby_however_numbered() {
    let ip = |text: &str| text.parse().unwrap();
    // A LAN with global IPv6, and one with public IPv4.
    let networks = [(ip("2001:db8:1::1"), 64), (ip("198.51.100.4"), 24)];
    assert!(!compresses(
        Compression::Auto,
        &direct("[2001:db8:1::2]:47011"),
        &networks
    ));
    assert!(!compresses(
        Compression::Auto,
        &direct("198.51.100.9:47011"),
        &networks
    ));
    assert!(compresses(
        Compression::Auto,
        &direct("[2001:db8:2::2]:47011"),
        &networks
    ));
    assert!(compresses(
        Compression::Auto,
        &direct("198.51.101.9:47011"),
        &networks
    ));
    // A prefix of 0 would make everything nearby.
    assert!(compresses(
        Compression::Auto,
        &direct("203.0.113.7:47011"),
        &[(ip("192.0.2.1"), 0)]
    ));
}
