//! Tests for `nix_pixie::bulk`: which of the target's addresses a copy may
//! take instead of the session's path.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::net::IpAddr;

use nix_pixie::bulk::{Via, candidates};
use nix_pixie_proto::inventory::Nic;

fn nic(name: &str, up: bool, addrs: &[&str]) -> Nic {
    Nic {
        name: name.into(),
        mac: "52:54:00:12:01:02".into(),
        link_up: up,
        addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(),
    }
}

fn ips(addrs: &[&str]) -> Vec<IpAddr> {
    addrs.iter().map(|a| a.parse().unwrap()).collect()
}

#[test]
fn other_nics_addresses_are_candidates_ipv4_first() {
    let nics = [
        // The session's: over the USB cable.
        nic("usb0", true, &["fe80::2", "fe80::1c2:3ff:fe04:506"]),
        nic(
            "eth0",
            true,
            &["fe80::5054:ff:fe12:102", "2001:db8::7", "10.0.3.195"],
        ),
        nic("eth1", false, &["192.168.9.9"]),
    ];
    assert_eq!(
        candidates(&nics, Some("fe80::2".parse().unwrap())),
        ips(&["10.0.3.195", "2001:db8::7"])
    );
    // The session goes to eth0 itself: nothing else is up.
    assert_eq!(
        candidates(&nics, Some("10.0.3.195".parse().unwrap())),
        Vec::<IpAddr>::new()
    );
    // Relayed: any address is another way.
    assert_eq!(candidates(&nics, None), ips(&["10.0.3.195", "2001:db8::7"]));
}

#[test]
fn bulk_via_is_auto_session_or_an_address() {
    assert_eq!("auto".parse::<Via>(), Ok(Via::Auto));
    assert_eq!("session".parse::<Via>(), Ok(Via::Session));
    assert_eq!(
        "10.0.3.195".parse::<Via>(),
        Ok(Via::Addr("10.0.3.195".parse().unwrap()))
    );
    assert!("eth0".parse::<Via>().is_err());
}
