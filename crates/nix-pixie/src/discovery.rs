//! Finding targets from the controller: DNS-SD, probes and the relay
//! rendezvous, merged.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt::{self, Write as _};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use iroh::{Endpoint, EndpointId, RelayUrl};
use mdns_sd::ServiceEvent;
use nix_pixie_proto::announce::{AnnounceError, Announcement, MAX_LABEL, PROBE_PORT, Pairing};
use nix_pixie_proto::code::NameplateSpace;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::mdns::{self, Mdns};
use crate::net::{Relays, sanitise};
use crate::probe::{Prober, Reply, RoundReport};
use crate::rendezvous::Search;

const PROBE_INTERVAL: Duration = Duration::from_millis(1500);

/// Which discovery mechanisms the controller uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Method {
    /// DNS-SD and probes.
    Both,
    /// DNS-SD only. Needs mDNS replies to get through this machine's firewall.
    Mdns,
    /// Probes only.
    Probe,
}

/// A target, with every address we have heard of so far.
#[derive(Debug, Clone)]
pub struct Peer {
    pub id: EndpointId,
    pub pairing: Pairing,
    /// Sanitised, safe to display.
    pub label: String,
    pub addrs: BTreeSet<SocketAddr>,
    /// The relays it may be connected to, if it answered at its rendezvous.
    pub relays: BTreeSet<RelayUrl>,
}

/// Counts for error messages.
#[derive(Debug, Default)]
pub struct Stats {
    pub mdns_records: usize,
    pub probe_replies: usize,
    pub rendezvous_answers: usize,
}

/// Finds announcing targets via DNS-SD, probes and (with
/// [`Self::search_relays`]) their rendezvous, and merges what they report
/// into one [`Peer`] per endpoint. Owns the mDNS daemon and the search
/// tasks; call `shutdown` when done.
pub struct Discovery {
    mdns: Option<(Mdns, mdns_sd::Receiver<ServiceEvent>)>,
    prober: Option<(
        Prober,
        tokio::task::JoinHandle<()>,
        watch::Receiver<RoundReport>,
    )>,
    rendezvous: Option<Search>,
    peers: HashMap<EndpointId, Peer>,
    /// News that arrived while [`Self::settle`] waited for another peer.
    pending: VecDeque<Peer>,
    /// Probing only these, not the local network.
    targets: Targets,
    /// Targets that speak another protocol version, by address: we don't
    /// pair with them, but say so.
    other_versions: BTreeMap<String, u32>,
    pub stats: Stats,
}

impl Discovery {
    /// Searches the local network with `method`, or with `targets`, probes
    /// only those addresses instead (see [`resolve_targets`]).
    pub async fn start(method: Method, targets: Targets) -> Result<Self> {
        let local = targets.is_empty();
        let mdns = match method {
            Method::Both | Method::Mdns if local => {
                let mdns = Mdns::new()?;
                let events = mdns.browse()?;
                Some((mdns, events))
            }
            _ => None,
        };
        let prober = if !local || matches!(method, Method::Both | Method::Probe) {
            let prober = Prober::new().await?;
            let (tx, rx) = watch::channel(RoundReport::default());
            let task = if local {
                prober.spawn_rounds(PROBE_INTERVAL, tx)
            } else {
                prober.spawn_targeted(targets.clone(), PROBE_INTERVAL, tx)
            };
            Some((prober, task, rx))
        } else {
            None
        };
        Ok(Self {
            mdns,
            prober,
            rendezvous: None,
            peers: HashMap::new(),
            pending: VecDeque::new(),
            targets,
            other_versions: BTreeMap::new(),
            stats: Stats::default(),
        })
    }

    /// Also asks at the relay rendezvous of `nameplate`, with `endpoint`
    /// through `relays`: this finds a target on another network that can't
    /// be reached from here, as long as both reach a relay.
    pub fn search_relays(&mut self, endpoint: &Endpoint, nameplate: u32, relays: &Relays) {
        self.rendezvous = Some(Search::spawn(endpoint.clone(), nameplate, relays));
    }

    /// Waits for news about a target: first sighting, or new addresses.
    pub async fn next(&mut self) -> Result<Peer> {
        match self.pending.pop_front() {
            Some(peer) => Ok(peer),
            None => self.receive().await,
        }
    }

    /// Collects addresses for `id` for `window`, then returns everything known
    /// about it. A target answers on several addresses (IPv4, IPv6, link-local,
    /// via probes and DNS-SD) within a few hundred milliseconds; dialing on the
    /// first one alone misses the better ones. News about other peers is kept
    /// for [`Self::next`].
    pub async fn settle(&mut self, id: EndpointId, window: Duration) -> Option<Peer> {
        let deadline = tokio::time::Instant::now() + window;
        while let Ok(Ok(peer)) = tokio::time::timeout_at(deadline, self.receive()).await {
            if peer.id != id {
                self.pending.push_back(peer);
            }
        }
        self.peers.get(&id).cloned()
    }

    async fn receive(&mut self) -> Result<Peer> {
        loop {
            let (announcement, addrs, relays, via) = tokio::select! {
                event = recv_mdns(self.mdns.as_ref()) => {
                    match event? {
                        ServiceEvent::ServiceResolved(service) => match mdns::parse(&service) {
                            Ok((announcement, addrs)) => {
                                self.stats.mdns_records += 1;
                                (announcement, addrs, BTreeSet::new(), "mDNS")
                            }
                            Err(e) => {
                                if let Some(AnnounceError::Version(version)) = e.downcast_ref() {
                                    self.other_version(&sanitise(&service.fullname, 100), *version);
                                } else {
                                    debug!(fullname = %service.fullname, "ignoring record: {e:#}");
                                }
                                continue;
                            }
                        },
                        _ => continue,
                    }
                }
                reply = recv_probe(self.prober.as_ref()) => match reply {
                    Reply::Target(announcement, addr) => {
                        self.stats.probe_replies += 1;
                        (announcement, vec![addr], BTreeSet::new(), "probe")
                    }
                    Reply::OtherVersion { from, version } => {
                        self.other_version(&from.ip().to_string(), version);
                        continue;
                    }
                },
                (announcement, relays) = recv_rendezvous(self.rendezvous.as_mut()) => {
                    self.stats.rendezvous_answers += 1;
                    (announcement, Vec::new(), relays, "relay")
                }
            };
            if let Some(peer) = self.merge(&announcement, addrs, relays, via) {
                return Ok(peer);
            }
        }
    }

    /// Remembers a target of another protocol version, and says so the first
    /// time.
    fn other_version(&mut self, at: &str, version: u32) {
        if self
            .other_versions
            .insert(at.to_string(), version)
            .is_none()
        {
            warn!(
                target = %at,
                "found a target, but {}",
                AnnounceError::Version(version)
            );
        }
    }

    /// Records an announcement; returns the peer if anything changed.
    fn merge(
        &mut self,
        announcement: &Announcement,
        addrs: Vec<SocketAddr>,
        relays: BTreeSet<RelayUrl>,
        via: &str,
    ) -> Option<Peer> {
        let id = EndpointId::from_bytes(&announcement.id).ok()?;
        let label = sanitise(&announcement.label, MAX_LABEL);
        let peer = self.peers.entry(id).or_insert_with(|| {
            info!(target = %id, %label, via, "found target");
            Peer {
                id,
                pairing: announcement.pairing,
                label: label.clone(),
                addrs: BTreeSet::new(),
                relays: BTreeSet::new(),
            }
        });
        let mut changed = peer.pairing != announcement.pairing || peer.label != label;
        peer.pairing = announcement.pairing;
        peer.label = label;
        for addr in addrs {
            changed |= peer.addrs.insert(addr);
        }
        for relay in relays {
            changed |= peer.relays.insert(relay);
        }
        changed.then(|| peer.clone())
    }

    /// Where we looked, for "nothing found" errors; and who answered in
    /// another protocol version.
    pub fn describe(&self) -> String {
        let mut text = self.looked();
        for (at, version) in &self.other_versions {
            let _ = write!(
                text,
                ". The target at {at} answered, but {}",
                AnnounceError::Version(*version)
            );
        }
        text
    }

    fn looked(&self) -> String {
        if !self.targets.is_empty() {
            let targets = self.targets.describe();
            return format!(
                "probed {}: {} reply(s). Is the agent running there, and does its firewall (and \
                 any in between) let UDP {} and {PROBE_PORT} in",
                targets.join(", "),
                self.stats.probe_replies,
                nix_pixie_proto::DEFAULT_PORT,
            );
        }
        let mut parts = Vec::new();
        if self.mdns.is_some() {
            parts.push(format!("DNS-SD: {} record(s)", self.stats.mdns_records));
        }
        if let Some((_, _, report)) = &self.prober {
            let report = report.borrow();
            parts.push(format!(
                "probes: {} reply(s) on {}",
                self.stats.probe_replies,
                if report.interfaces.is_empty() {
                    "no interfaces".to_string()
                } else {
                    report.interfaces.join(", ")
                }
            ));
            if !report.neighbours_ok {
                parts.push(
                    "the neighbour table was unreadable (is iproute2 installed?)".to_string(),
                );
            }
        }
        if let Some(search) = &self.rendezvous {
            parts.push(search.describe());
        }
        let mut text = parts.join("; ");
        if self.prober.is_none() && self.stats.mdns_records == 0 {
            text.push_str(
                ". No DNS-SD records arrived at all: this machine's firewall probably drops mDNS \
                 replies (UDP 5353). Use the default discovery, which also probes",
            );
        }
        text
    }

    /// What to check when no target showed `nameplate`, given where we
    /// looked.
    pub fn hint(&self, nameplate: u32) -> &'static str {
        if !self.targets.is_empty() {
            "Check that the target shows this code"
        } else if self.rendezvous.is_some() {
            "Check that the target shows this code, and that it is on this network or reaches the same relay"
        } else if NameplateSpace::of(nameplate) == Some(NameplateSpace::Relay) {
            "Check that the target shows this code and is on this network. Without a relay (--no-relay), a target elsewhere is only found with --target HOST"
        } else {
            "Check that the target shows this code and is on this network. A code this short is from a target that uses no relay: elsewhere, it is only found with --target HOST"
        }
    }

    pub fn shutdown(self) {
        if let Some((mdns, _)) = self.mdns {
            mdns.shutdown();
        }
        if let Some((_, task, _)) = self.prober {
            task.abort();
        }
        if let Some(search) = self.rendezvous {
            search.shutdown();
        }
    }
}

impl fmt::Debug for Discovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Discovery")
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

async fn recv_mdns(mdns: Option<&(Mdns, mdns_sd::Receiver<ServiceEvent>)>) -> Result<ServiceEvent> {
    match mdns {
        Some((_, events)) => Ok(events.recv_async().await?),
        None => std::future::pending().await,
    }
}

async fn recv_probe(
    prober: Option<&(
        Prober,
        tokio::task::JoinHandle<()>,
        watch::Receiver<RoundReport>,
    )>,
) -> Reply {
    match prober {
        Some((prober, _, _)) => prober.recv().await,
        None => std::future::pending().await,
    }
}

async fn recv_rendezvous(search: Option<&mut Search>) -> (Announcement, BTreeSet<RelayUrl>) {
    match search {
        Some(search) => {
            let announcement = search.recv().await;
            (announcement, search.relays.clone())
        }
        None => std::future::pending().await,
    }
}

/// Where network interfaces are listed, by name, with their index.
pub const SYS_NET: &str = "/sys/class/net";

/// The hosts given with `--target`, as addresses to probe. A link-local
/// one's interface is looked up again for every probe: a USB link that
/// drops comes back as a new interface with a new index, and it may not be
/// there yet when the controller starts (the device is still booting).
#[derive(Debug, Clone, Default)]
pub struct Targets {
    fixed: Vec<SocketAddr>,
    /// Link-local addresses, with the name of their interface.
    scoped: Vec<(Ipv6Addr, String)>,
    sys_net: PathBuf,
}

impl Targets {
    pub fn is_empty(&self) -> bool {
        self.fixed.is_empty() && self.scoped.is_empty()
    }

    /// Where to probe now. Interfaces that aren't there are left out.
    #[must_use]
    pub fn now(&self) -> Vec<SocketAddr> {
        let mut addrs = self.fixed.clone();
        for (ip, interface) in &self.scoped {
            if let Ok(scope) = interface_index(interface, &self.sys_net) {
                addrs.push(SocketAddr::V6(SocketAddrV6::new(*ip, PROBE_PORT, 0, scope)));
            }
        }
        addrs
    }

    /// The targets as given, for messages, saying which interface isn't
    /// there.
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        let fixed = self.fixed.iter().map(ToString::to_string);
        let scoped = self.scoped.iter().map(|(ip, interface)| {
            if interface_index(interface, &self.sys_net).is_ok() {
                format!("{ip}%{interface}")
            } else {
                format!("{ip}%{interface} (no interface {interface} yet)")
            }
        });
        fixed.chain(scoped).collect()
    }
}

/// The probe addresses of `hosts` (`--target`): IP addresses, or names to
/// look up.
pub async fn resolve_targets(hosts: &[String]) -> Result<Targets> {
    resolve_targets_in(hosts, Path::new(SYS_NET)).await
}

/// [`resolve_targets`], with interfaces under `sys_net`.
pub async fn resolve_targets_in(hosts: &[String], sys_net: &Path) -> Result<Targets> {
    let mut targets = Targets {
        sys_net: sys_net.to_path_buf(),
        ..Targets::default()
    };
    for host in hosts {
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        match literal(bare, sys_net)? {
            Some(Literal::Addr(addr)) => targets.fixed.push(addr),
            Some(Literal::LinkLocal(ip, interface)) => {
                if interface_index(&interface, sys_net).is_err() {
                    info!(%interface, "no interface {interface} yet; probing {bare} once it is there");
                }
                if !targets.scoped.contains(&(ip, interface.clone())) {
                    targets.scoped.push((ip, interface));
                }
            }
            None => {
                let found: Vec<SocketAddr> = tokio::net::lookup_host((bare, PROBE_PORT))
                    .await
                    .with_context(|| format!("looking up {host}"))?
                    .collect();
                if found.is_empty() {
                    bail!("{host} has no addresses");
                }
                targets.fixed.extend(found);
            }
        }
    }
    targets.fixed.sort_unstable();
    targets.fixed.dedup();
    Ok(targets)
}

/// An IP address given as a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    /// Its probe address.
    Addr(SocketAddr),
    /// A link-local address on the interface of this name.
    LinkLocal(Ipv6Addr, String),
}

/// `host`, if it is an IP address. An IPv6 link-local one needs its
/// interface, by name or index (`fe80::2%usb0`): the same address can be on
/// every link. An index is looked up under `sys_net` (`/sys/class/net`),
/// where interfaces show up before they have addresses.
pub fn literal(host: &str, sys_net: &Path) -> Result<Option<Literal>> {
    let (ip, zone) = match host.split_once('%') {
        Some((ip, zone)) => (ip, Some(zone)),
        None => (host, None),
    };
    let Ok(ip) = ip.parse::<IpAddr>() else {
        return Ok(None);
    };
    let IpAddr::V6(v6) = ip else {
        if zone.is_some() {
            bail!("{host}: only IPv6 link-local addresses take an interface");
        }
        return Ok(Some(Literal::Addr(SocketAddr::new(ip, PROBE_PORT))));
    };
    let scope = match zone {
        None if v6.is_unicast_link_local() => bail!(
            "{host} is a link-local address, which every link has: add the interface the \
             target is on, e.g. {host}%eth0"
        ),
        None => 0,
        Some(zone) if zone.parse::<u32>().is_err() => {
            if !is_interface_name(zone) {
                bail!("--target {host}: {zone:?} is no interface's name");
            }
            return Ok(Some(Literal::LinkLocal(v6, zone.to_string())));
        }
        Some(zone) => interface_index(zone, sys_net).with_context(|| format!("--target {host}"))?,
    };
    Ok(Some(Literal::Addr(SocketAddr::V6(SocketAddrV6::new(
        v6, PROBE_PORT, 0, scope,
    )))))
}

/// Whether `name` can be a network interface's (and a file's, under
/// `/sys/class/net`).
fn is_interface_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@'))
}

/// The index of the network interface `zone` (a name, or already an index).
fn interface_index(zone: &str, sys_net: &Path) -> Result<u32> {
    if let Ok(index) = zone.parse::<u32>() {
        return Ok(index);
    }
    if is_interface_name(zone)
        && let Ok(text) = std::fs::read_to_string(sys_net.join(zone).join("ifindex"))
    {
        return text
            .trim()
            .parse()
            .with_context(|| format!("the index of interface {zone}"));
    }
    let mut known: Vec<String> = std::fs::read_dir(sys_net)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    known.sort();
    bail!(
        "there is no network interface {zone:?} (there are: {}); is the cable plugged in?",
        known.join(", ")
    )
}

/// The names of the interfaces that `addrs`' link-local addresses are on,
/// by index under `sys_net`: to find them again under a new index (see
/// [`rescoped`]).
#[must_use]
pub fn interface_names(
    addrs: impl IntoIterator<Item = SocketAddr>,
    sys_net: &Path,
) -> BTreeMap<u32, String> {
    let scopes: BTreeSet<u32> = addrs
        .into_iter()
        .filter_map(|addr| match addr {
            SocketAddr::V6(v6) if v6.ip().is_unicast_link_local() && v6.scope_id() != 0 => {
                Some(v6.scope_id())
            }
            _ => None,
        })
        .collect();
    let mut names = BTreeMap::new();
    if scopes.is_empty() {
        return names;
    }
    for entry in std::fs::read_dir(sys_net).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(index) = interface_index(&name, sys_net)
            && scopes.contains(&index)
        {
            names.insert(index, name);
        }
    }
    names
}

/// `addrs`' link-local addresses again, on interfaces of `names` that are
/// back under another index: a USB link the host enumerated anew.
#[must_use]
pub fn rescoped(
    addrs: &[SocketAddr],
    names: &BTreeMap<u32, String>,
    sys_net: &Path,
) -> Vec<SocketAddr> {
    addrs
        .iter()
        .filter_map(|addr| {
            let SocketAddr::V6(v6) = addr else {
                return None;
            };
            let name = names.get(&v6.scope_id())?;
            let now = interface_index(name, sys_net).ok()?;
            (now != v6.scope_id())
                .then(|| SocketAddr::V6(SocketAddrV6::new(*v6.ip(), v6.port(), 0, now)))
        })
        .collect()
}

/// Addresses worth dialing or probing.
pub fn is_usable(ip: IpAddr) -> bool {
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || matches!(ip, IpAddr::V4(v4) if v4.is_broadcast()))
}
