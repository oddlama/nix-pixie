//! Tests for `nix_pixie::probe`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use nix_pixie::probe::*;
use nix_pixie_proto::announce::{Announcement, PROBE_PORT};
use tokio::sync::watch;

#[test]
fn parses_neighbour_table() {
    let json = br#"[
        {"dst":"192.168.1.2","dev":"eth1","lladdr":"52:54:00:12:01:02","state":["REACHABLE"]},
        {"dst":"192.168.1.9","dev":"eth1","state":["FAILED"]},
        {"dst":"fe80::5054:ff:fe12:102","dev":"eth1","lladdr":"52:54:00:12:01:02","router":null,"state":["STALE"]},
        {"dst":"fe80::1","dev":"gone0","state":["STALE"]},
        {"dst":"2001:db8::7","dev":"eth1","state":["DELAY"]},
        {"dst":"ff02::1","dev":"eth1","state":["NOARP"]}
    ]"#;
    let parsed = parse_neighbours(json, |dev| (dev == "eth1").then_some(3));
    let expected: Vec<SocketAddr> = vec![
        "192.168.1.2:0".parse().unwrap(),
        SocketAddrV6::new("fe80::5054:ff:fe12:102".parse().unwrap(), 0, 0, 3).into(),
        "[2001:db8::7]:0".parse().unwrap(),
    ];
    assert_eq!(parsed, expected);
}

#[test]
fn computes_subnet_broadcast() {
    let b = |ip: &str, mask: &str| {
        subnet_broadcast(ip.parse().unwrap(), mask.parse().unwrap()).to_string()
    };
    assert_eq!(b("192.168.1.1", "255.255.255.0"), "192.168.1.255");
    assert_eq!(b("10.1.2.3", "255.255.0.0"), "10.1.255.255");
    assert_eq!(b("169.254.7.7", "255.255.0.0"), "169.254.255.255");
    assert_eq!(b("192.168.1.5", "255.255.255.252"), "192.168.1.7");
}

#[test]
fn garbage_neighbour_output_is_empty() {
    assert_eq!(
        parse_neighbours(b"not json", |_| None),
        Vec::<SocketAddr>::new()
    );
}

#[test]
fn canonicalises_mapped_addresses() {
    let mapped: SocketAddr = "[::ffff:192.168.1.2]:5".parse().unwrap();
    assert_eq!(canonical(mapped), "192.168.1.2:5".parse().unwrap());
    let v6: SocketAddr = "[fe80::1]:5".parse().unwrap();
    assert_eq!(canonical(v6), v6);
}

/// End to end on loopback: a responder answers a unicast probe.
#[tokio::test]
async fn responder_answers_probes_while_announcing() {
    use nix_pixie_proto::announce::Pairing;
    let announcement = Announcement::new([7; 32], 4242, Pairing::ShowsCode { nameplate: 5 }, "t");
    let (tx, rx) = watch::channel(Some(announcement.clone()));
    let responder = tokio::spawn(Responder::bind().await.unwrap().run(rx));
    let prober = Prober::new().await.unwrap();

    let reply = async |to| {
        loop {
            prober.probe(to).await;
            if let Ok(r) = tokio::time::timeout(Duration::from_millis(100), prober.recv()).await {
                return r;
            }
        }
    };
    let to = SocketAddr::from((Ipv4Addr::LOCALHOST, PROBE_PORT));
    let (got, from) = tokio::time::timeout(Duration::from_secs(5), reply(to))
        .await
        .unwrap();
    assert_eq!(got, announcement);
    assert_eq!(from, SocketAddr::from((Ipv4Addr::LOCALHOST, 4242)));

    // From the address probed, though the kernel would pick 127.0.0.1 to
    // reply to 127.0.0.1: a reply from elsewhere is nothing the prober sent
    // to, and a stateful firewall in front of it drops that.
    let other = Ipv4Addr::new(127, 0, 0, 2);
    let (_, from) = tokio::time::timeout(
        Duration::from_secs(5),
        reply(SocketAddr::from((other, PROBE_PORT))),
    )
    .await
    .unwrap();
    assert_eq!(from, SocketAddr::from((other, 4242)));

    // Paired targets stop answering.
    tx.send_replace(None);
    prober.probe(to).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), prober.recv())
            .await
            .is_err()
    );
    responder.abort();
}
