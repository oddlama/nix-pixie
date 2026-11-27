//! Tests for `nix_pixie::discovery`: `--target` addresses, and link-local
//! ones on interfaces that come and go.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::collections::BTreeMap;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};
use std::path::{Path, PathBuf};

use nix_pixie::discovery::{Literal, interface_names, literal, rescoped, resolve_targets_in};
use nix_pixie_proto::announce::PROBE_PORT;

/// A fake /sys/class/net with these interfaces and indices.
fn sys_net(test: &str, interfaces: &[(&str, u32)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nix-pixie-test-sys-net-{test}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    for (name, index) in interfaces {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(dir.join(name).join("ifindex"), format!("{index}\n")).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn on(ip: &str, port: u16, scope: u32) -> SocketAddr {
    SocketAddr::V6(SocketAddrV6::new(ip.parse().unwrap(), port, 0, scope))
}

#[test]
fn link_local_targets_keep_their_interface_by_name() {
    let net = sys_net("literal", &[("usb0", 7)]);
    let gadget: Ipv6Addr = "fe80::2".parse().unwrap();
    assert_eq!(
        literal("fe80::2%usb0", &net).unwrap(),
        Some(Literal::LinkLocal(gadget, "usb0".into()))
    );
    // Not there yet: looked up later, not refused now.
    assert_eq!(
        literal("fe80::2%usb1", &net).unwrap(),
        Some(Literal::LinkLocal(gadget, "usb1".into()))
    );
    assert_eq!(
        literal("fe80::2%7", &net).unwrap(),
        Some(Literal::Addr(on("fe80::2", PROBE_PORT, 7)))
    );
    assert_eq!(
        literal("192.0.2.1", &net).unwrap(),
        Some(Literal::Addr(SocketAddr::from((
            [192, 0, 2, 1],
            PROBE_PORT
        ))))
    );
    assert_eq!(literal("target.example", &net).unwrap(), None);
    assert!(literal("fe80::2", &net).is_err());
    assert!(literal("fe80::2%../etc", &net).is_err());
    assert!(literal("192.0.2.1%usb0", &net).is_err());
}

#[tokio::test]
async fn targets_follow_their_interface() {
    let net = sys_net("targets", &[("eth0", 2)]);
    let targets = resolve_targets_in(&["fe80::2%usb0".into(), "[2001:db8::1]".into()], &net)
        .await
        .unwrap();
    assert_eq!(targets.now(), vec![on("2001:db8::1", PROBE_PORT, 0)]);
    assert!(
        targets
            .describe()
            .contains(&"fe80::2%usb0 (no interface usb0 yet)".to_string()),
        "{:?}",
        targets.describe()
    );

    // The device booted: the host's interface shows up.
    std::fs::create_dir_all(net.join("usb0")).unwrap();
    std::fs::write(net.join("usb0/ifindex"), "9\n").unwrap();
    assert!(targets.now().contains(&on("fe80::2", PROBE_PORT, 9)));
    // It was enumerated anew, with another index.
    std::fs::write(net.join("usb0/ifindex"), "12\n").unwrap();
    assert!(targets.now().contains(&on("fe80::2", PROBE_PORT, 12)));
    assert!(targets.describe().contains(&"fe80::2%usb0".to_string()));
}

#[test]
fn link_local_addresses_move_with_their_interface() {
    let net = sys_net("rescope", &[("usb0", 9), ("eth0", 2)]);
    let addrs = [
        on("fe80::2", 47011, 9),
        on("fe80::1234", 47011, 2),
        on("2001:db8::1", 47011, 0),
    ];
    let names = interface_names(addrs, &net);
    assert_eq!(
        names,
        BTreeMap::from([(2, "eth0".to_string()), (9, "usb0".to_string())])
    );
    assert_eq!(rescoped(&addrs, &names, &net), Vec::<SocketAddr>::new());

    std::fs::write(net.join("usb0/ifindex"), "12\n").unwrap();
    assert_eq!(
        rescoped(&addrs, &names, &net),
        vec![on("fe80::2", 47011, 12)]
    );
    // Gone, not back yet.
    std::fs::remove_dir_all(net.join("usb0")).unwrap();
    assert_eq!(rescoped(&addrs, &names, &net), Vec::<SocketAddr>::new());
    assert_eq!(
        interface_names(addrs, Path::new("/nonexistent")),
        BTreeMap::new()
    );
}
