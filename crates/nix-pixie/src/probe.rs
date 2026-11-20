//! Firewall-friendly discovery with probes on UDP [`PROBE_PORT`].
//!
//! The controller runs on the operator's machine, whose firewall we can't
//! change. A stateful firewall accepts only replies that match a flow the
//! machine started: same addresses and ports, reversed. Replies to multicast
//! or broadcast never match, because they come from the responder's unicast
//! address. So the controller sends probes to *unicast* addresses. But which
//! addresses?
//!
//! The kernel's neighbour table tells us. We first send one probe to the
//! broadcast address (IPv4) and to all-nodes `ff02::1` (IPv6). A target answers
//! from its unicast address. Before it can, its kernel has to resolve our MAC
//! address with an ARP request or IPv6 neighbour solicitation. Our kernel
//! records the sender of that request in its neighbour table (RFC 826 / RFC
//! 4861 behaviour; Linux does). The firewall may drop the reply itself, but
//! the neighbour entry stays. We then probe every neighbour by unicast, and
//! those replies pass.
//!
//! One catch: if the target still has *us* in its neighbour cache, it doesn't
//! need to ask, and we learn nothing until its entry goes stale (5-10 s).
//! That happens whenever the controller's side flushes its table, e.g.
//! NetworkManager re-activating a link. So the target (root on the installer)
//! flushes its entry for the prober before replying, which forces the
//! request.
//!
//! This sends a handful of packets, not a subnet sweep. The controller needs
//! no privileges, only `ip` (iproute2) to read the neighbour table.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use if_addrs::IfAddr;
use nix_pixie_proto::announce::{
    Announcement, Nonce, PROBE_LEN, PROBE_PORT, decode_probe, decode_reply, encode_probe,
    encode_reply,
};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tracing::{debug, warn};

/// Path to `ip`; the Nix build pins it to iproute2 from the store.
const IP: &str = match option_env!("NIX_PIXIE_IP") {
    Some(path) => path,
    None => "ip",
};

/// Pause between the broadcast probe and the unicast probes, so targets have
/// time to resolve our address (which creates the neighbour entries).
const NEIGHBOUR_DELAY: Duration = Duration::from_millis(300);
/// Replies per second a target sends at most.
const MAX_REPLIES_PER_SECOND: u32 = 50;
/// How often a target flushes its neighbour entry for the same prober.
const FLUSH_INTERVAL: Duration = Duration::from_secs(10);
/// How many probers' flush times a target remembers.
const MAX_FLUSH_ENTRIES: usize = 256;

/// The target's side: answers probes with the current announcement.
pub struct Responder {
    socket: UdpSocket,
}

impl Responder {
    /// Binds the probe port, so conflicts fail at startup.
    pub async fn bind() -> Result<Self> {
        // Dual-stack: IPv4 probes arrive as v4-mapped addresses.
        let socket = match UdpSocket::bind((Ipv6Addr::UNSPECIFIED, PROBE_PORT)).await {
            Ok(socket) => socket,
            Err(_) => UdpSocket::bind((Ipv4Addr::UNSPECIFIED, PROBE_PORT))
                .await
                .with_context(|| format!("binding probe port {PROBE_PORT}"))?,
        };
        Ok(Self { socket })
    }

    /// Answers until dropped. Sends nothing while `announcement` is `None`
    /// (e.g. while paired).
    pub async fn run(self, announcement: watch::Receiver<Option<Announcement>>) {
        let mut buf = vec![0; PROBE_LEN + 1];
        let mut window = (Instant::now(), 0u32);
        let mut flushed: HashMap<IpAddr, Instant> = HashMap::new();
        loop {
            let Ok((len, from)) = self.socket.recv_from(&mut buf).await else {
                continue;
            };
            let Some(nonce) = decode_probe(&buf[..len]) else {
                continue;
            };
            debug!(%from, "probe received");
            if window.0.elapsed() >= Duration::from_secs(1) {
                window = (Instant::now(), 0);
            }
            if window.1 >= MAX_REPLIES_PER_SECOND {
                continue;
            }
            window.1 += 1;
            let reply = announcement
                .borrow()
                .as_ref()
                .map(|a| encode_reply(&nonce, a));
            let Some(reply) = reply else { continue };
            let prober = canonical(from).ip();
            if flushed
                .get(&prober)
                .is_none_or(|at| at.elapsed() >= FLUSH_INTERVAL)
            {
                if flushed.len() >= MAX_FLUSH_ENTRIES {
                    flushed.retain(|_, at| at.elapsed() < FLUSH_INTERVAL);
                }
                if flushed.len() < MAX_FLUSH_ENTRIES {
                    flushed.insert(prober, Instant::now());
                    forget_neighbour(prober).await;
                }
            }
            if let Err(e) = self.socket.send_to(&reply, from).await {
                debug!(%from, "probe reply: {e}");
            }
        }
    }
}

/// The controller's side: sends probes and receives replies.
pub struct Prober {
    v4: Arc<UdpSocket>,
    v6: Option<Arc<UdpSocket>>,
    nonce: Nonce,
}

/// What a probe round looked at, for error messages.
#[derive(Debug, Clone, Default)]
pub struct RoundReport {
    /// Interface names with their addresses.
    pub interfaces: Vec<String>,
    /// Whether the neighbour table could be read.
    pub neighbours_ok: bool,
}

impl Prober {
    pub async fn new() -> Result<Self> {
        let v4 = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
        v4.set_broadcast(true)?;
        let v6 = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).await.ok();
        Ok(Self {
            v4: Arc::new(v4),
            v6: v6.map(Arc::new),
            nonce: nix_pixie_proto::announce::new_nonce(),
        })
    }

    /// Runs probe rounds every `interval`, reporting each round on `report`.
    pub fn spawn_rounds(
        &self,
        interval: Duration,
        report: watch::Sender<RoundReport>,
    ) -> tokio::task::JoinHandle<()> {
        let (v4, v6, probe) = (self.v4.clone(), self.v6.clone(), encode_probe(&self.nonce));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                let round = round(&v4, v6.as_deref(), &probe).await;
                report.send_replace(round);
            }
        })
    }

    /// Next valid reply: the announcement and the address it came from (with
    /// the iroh port from the announcement).
    pub async fn recv(&self) -> (Announcement, SocketAddr) {
        loop {
            let received = match &self.v6 {
                Some(v6) => tokio::select! {
                    r = recv_reply(&self.v4, &self.nonce) => r,
                    r = recv_reply(v6, &self.nonce) => r,
                },
                None => recv_reply(&self.v4, &self.nonce).await,
            };
            if let Some(reply) = received {
                return reply;
            }
        }
    }
}

/// One datagram from `socket`, if it is a valid reply to our probe.
async fn recv_reply(socket: &UdpSocket, nonce: &Nonce) -> Option<(Announcement, SocketAddr)> {
    let mut buf = vec![0; PROBE_LEN];
    let (len, from) = socket.recv_from(&mut buf).await.ok()?;
    match decode_reply(&buf[..len], nonce) {
        Ok(announcement) => {
            debug!(%from, "probe reply");
            let mut addr = canonical(from);
            addr.set_port(announcement.port);
            Some((announcement, addr))
        }
        Err(e) => {
            debug!(%from, "ignoring probe reply: {e}");
            None
        }
    }
}

async fn round(v4: &UdpSocket, v6: Option<&UdpSocket>, probe: &[u8]) -> RoundReport {
    let mut report = RoundReport::default();
    let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
    let mut v6_indexes = Vec::new();
    for iface in interfaces
        .iter()
        .filter(|i| !i.is_loopback() && i.is_oper_up())
    {
        match &iface.addr {
            IfAddr::V4(a) => {
                report
                    .interfaces
                    .push(format!("{} {}/{}", iface.name, a.ip, a.prefixlen));
                if a.prefixlen < 31 {
                    // Not `a.broadcast`: for addresses configured without one
                    // (`ip addr add 192.168.1.1/24`), getifaddrs reports the
                    // interface's own address there.
                    let broadcast = subnet_broadcast(a.ip, a.netmask);
                    send(v4, SocketAddr::from((broadcast, PROBE_PORT)), probe).await;
                }
            }
            IfAddr::V6(a) => {
                report
                    .interfaces
                    .push(format!("{} {}/{}", iface.name, a.ip, a.prefixlen));
                if let Some(index) = iface.index
                    && !v6_indexes.contains(&index)
                {
                    v6_indexes.push(index);
                }
            }
        }
    }
    if let Some(v6) = v6 {
        for index in v6_indexes {
            let all_nodes = SocketAddrV6::new(
                Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1),
                PROBE_PORT,
                0,
                index,
            );
            send(v6, all_nodes.into(), probe).await;
        }
    }

    tokio::time::sleep(NEIGHBOUR_DELAY).await;
    match neighbours().await {
        Ok(neighbours) => {
            debug!(?neighbours, "probing neighbours");
            report.neighbours_ok = true;
            for mut addr in neighbours {
                addr.set_port(PROBE_PORT);
                match (addr, v6) {
                    (SocketAddr::V4(_), _) => send(v4, addr, probe).await,
                    (SocketAddr::V6(_), Some(v6)) => send(v6, addr, probe).await,
                    (SocketAddr::V6(_), None) => {}
                }
            }
        }
        Err(e) => debug!("reading the neighbour table: {e:#}"),
    }
    report
}

/// Drops our neighbour entry for `ip`, so replying to it makes the kernel send
/// an ARP request or neighbour solicitation, which recreates the prober's
/// entry for us. Needs root; without it this does nothing.
async fn forget_neighbour(ip: IpAddr) {
    let flush = tokio::process::Command::new(IP)
        .args(["neigh", "flush", "to", &ip.to_string()])
        .output();
    match tokio::time::timeout(Duration::from_secs(1), flush).await {
        Ok(Ok(output)) if output.status.success() => {}
        other => debug!(%ip, "flushing neighbour entry: {other:?}"),
    }
}

async fn send(socket: &UdpSocket, to: SocketAddr, probe: &[u8]) {
    if let Err(e) = socket.send_to(probe, to).await {
        debug!(%to, "sending probe: {e}");
    }
}

/// Neighbour addresses (port 0) from `ip -j neigh`, with interface scope for
/// link-local IPv6.
async fn neighbours() -> Result<Vec<SocketAddr>> {
    let output = tokio::process::Command::new(IP)
        .args(["-json", "neigh", "show"])
        .output()
        .await
        .with_context(|| format!("running `{IP}`"))?;
    anyhow::ensure!(
        output.status.success(),
        "`{IP} neigh` failed: {}",
        output.status
    );
    Ok(parse_neighbours(&output.stdout, |dev| {
        std::fs::read_to_string(format!("/sys/class/net/{dev}/ifindex"))
            .ok()
            .and_then(|i| i.trim().parse().ok())
    }))
}

fn parse_neighbours(json: &[u8], ifindex: impl Fn(&str) -> Option<u32>) -> Vec<SocketAddr> {
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_slice(json) else {
        warn!("unexpected output from `{IP} -json neigh`");
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let states = entry.get("state")?.as_array()?;
            if states
                .iter()
                .any(|s| matches!(s.as_str(), Some("FAILED" | "INCOMPLETE")))
            {
                return None;
            }
            let ip: IpAddr = entry.get("dst")?.as_str()?.parse().ok()?;
            if !crate::discovery::is_usable(ip) {
                return None;
            }
            Some(match ip {
                IpAddr::V6(v6) if v6.is_unicast_link_local() => {
                    let index = ifindex(entry.get("dev")?.as_str()?)?;
                    SocketAddr::V6(SocketAddrV6::new(v6, 0, 0, index))
                }
                ip => SocketAddr::new(ip, 0),
            })
        })
        .collect()
}

/// The directed broadcast address of `ip`'s subnet.
fn subnet_broadcast(ip: Ipv4Addr, netmask: Ipv4Addr) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(ip) | !u32::from(netmask))
}

/// Turns v4-mapped IPv6 addresses (from dual-stack sockets) back into IPv4.
fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::from((v4, v6.port())),
            None => addr,
        },
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(parse_neighbours(b"not json", |_| None).is_empty());
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
        let announcement =
            Announcement::new([7; 32], 4242, Pairing::ShowsCode { nameplate: 5 }, "t");
        let (tx, rx) = watch::channel(Some(announcement.clone()));
        let responder = tokio::spawn(Responder::bind().await.unwrap().run(rx));
        let prober = Prober::new().await.unwrap();
        let probe = encode_probe(&prober.nonce);

        let to = SocketAddr::from((Ipv4Addr::LOCALHOST, PROBE_PORT));
        let reply = async {
            loop {
                send(&prober.v4, to, &probe).await;
                if let Ok(r) = tokio::time::timeout(Duration::from_millis(100), prober.recv()).await
                {
                    return r;
                }
            }
        };
        let (got, from) = tokio::time::timeout(Duration::from_secs(5), reply)
            .await
            .unwrap();
        assert_eq!(got, announcement);
        assert_eq!(from, SocketAddr::from((Ipv4Addr::LOCALHOST, 4242)));

        // Paired targets stop answering.
        tx.send_replace(None);
        send(&prober.v4, to, &probe).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(300), prober.recv())
                .await
                .is_err()
        );
        responder.abort();
    }
}
