//! Tests for `nix_pixie::net`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};

use iroh::{SecretKey, TransportAddr};
use nix_pixie::discovery::Peer;
use nix_pixie::net::{Relays, dial_addrs, is_own, own_addresses, routes, sanitise};
use nix_pixie_proto::announce::Pairing;

fn args(urls: &[&str], disabled: bool) -> anyhow::Result<Relays> {
    let urls: Vec<String> = urls.iter().map(ToString::to_string).collect();
    Relays::from_args(&urls, disabled)
}

#[test]
fn relays_default_to_n0s() {
    let relays = args(&[], false).unwrap();
    assert_eq!(relays, Relays::N0);
    assert!(relays.is_enabled());
    assert!(!relays.urls().is_empty());
    assert_eq!(relays.to_string(), "n0's public relays");
}

#[test]
fn relays_can_be_turned_off() {
    let relays = args(&[], true).unwrap();
    assert_eq!(relays, Relays::Disabled);
    assert!(!relays.is_enabled());
    assert!(relays.urls().is_empty());
}

#[test]
fn custom_relays_replace_the_default() {
    let relays = args(
        &["https://relay.example.org", "http://192.168.1.2:3340"],
        false,
    )
    .unwrap();
    let urls: Vec<String> = relays.urls().iter().map(ToString::to_string).collect();
    assert_eq!(
        urls,
        ["http://192.168.1.2:3340/", "https://relay.example.org/"]
    );
    assert_eq!(
        relays.to_string(),
        "http://192.168.1.2:3340/, https://relay.example.org/"
    );
}

#[test]
fn relay_urls_must_be_http_urls() {
    for url in [
        "relay.example.org",
        "ftp://relay.example.org",
        "",
        "https://",
    ] {
        let error = args(&[url], false).unwrap_err().to_string();
        assert!(error.contains("is not a relay URL"), "{url}: {error}");
    }
}

#[test]
fn routes_are_addresses_and_relays() {
    let relays = args(&["https://relay.example.org"], false).unwrap();
    let addr: SocketAddr = "192.168.1.2:47011".parse().unwrap();
    let mut peer = Peer {
        id: SecretKey::from_bytes(&[7; 32]).public(),
        pairing: Pairing::ShowsCode { nameplate: 482_913 },
        label: "target".into(),
        addrs: BTreeSet::from([addr]),
        relays: relays.urls(),
    };
    let relay = relays.urls().into_iter().next().unwrap();
    assert_eq!(
        routes(&peer),
        [TransportAddr::Ip(addr), TransportAddr::Relay(relay.clone())]
    );
    // Found at its rendezvous only: nothing but the relay to dial.
    peer.addrs.clear();
    assert_eq!(routes(&peer), [TransportAddr::Relay(relay)]);
}

#[test]
fn an_address_is_the_machines_own_or_a_nats() {
    let own: Vec<IpAddr> = ["192.168.1.7", "fe80::7", "2001:db8::7"]
        .iter()
        .map(|a| a.parse().unwrap())
        .collect();
    let is = |addr: &str| is_own(addr.parse().unwrap(), &own);
    assert!(is("192.168.1.7:47011"));
    assert!(is("[2001:db8::7]:47011"));
    // The scope is ours, not the machine's.
    assert!(is("[fe80::7%3]:47011"));
    // The same address, as an IPv6 socket reports an IPv4 peer.
    assert!(is("[::ffff:192.168.1.7]:47011"));
    // What a NAT in front of it shows instead.
    assert!(!is("203.0.113.9:61001"));
    assert!(!is("[2001:db8::8]:47011"));
    // A target that tells nothing has no address anybody may trust.
    assert!(!is_own("192.168.1.7:47011".parse().unwrap(), &[]));
}

#[test]
fn own_addresses_leave_out_loopback() {
    assert!(own_addresses().iter().all(|a| !a.is_loopback()));
}

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

#[test]
fn sanitise_strips_text_reordering_and_invisible_characters() {
    // "nvme0n1" that would display as "nvme0n1" followed by reversed text.
    assert_eq!(sanitise("disk\u{202E}1n0emvn\u{202C}", 63), "disk1n0emvn");
    assert_eq!(sanitise("we\u{200B}b1\u{2066}\u{FEFF}", 63), "web1");
    // Ordinary non-ASCII text stays.
    assert_eq!(sanitise("Größe ✓", 63), "Größe ✓");
}
