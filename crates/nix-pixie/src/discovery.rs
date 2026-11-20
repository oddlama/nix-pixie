//! Finding targets from the controller: DNS-SD and probes, merged.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::Result;
use iroh::EndpointId;
use mdns_sd::ServiceEvent;
use nix_pixie_proto::announce::{Announcement, MAX_LABEL, Pairing};
use tokio::sync::watch;
use tracing::{debug, info};

use crate::mdns::{self, Mdns};
use crate::net::sanitise;
use crate::probe::{Prober, RoundReport};

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
}

/// Counts for error messages.
#[derive(Debug, Default)]
pub struct Stats {
    pub mdns_records: usize,
    pub probe_replies: usize,
}

/// Finds announcing targets via DNS-SD and probes, and merges what both
/// report into one [`Peer`] per endpoint. Owns the mDNS daemon and the probe
/// task; call `shutdown` when done.
pub struct Discovery {
    mdns: Option<(Mdns, mdns_sd::Receiver<ServiceEvent>)>,
    prober: Option<(
        Prober,
        tokio::task::JoinHandle<()>,
        watch::Receiver<RoundReport>,
    )>,
    peers: HashMap<EndpointId, Peer>,
    /// News that arrived while [`Self::settle`] waited for another peer.
    pending: VecDeque<Peer>,
    pub stats: Stats,
}

impl Discovery {
    pub async fn start(method: Method) -> Result<Self> {
        let mdns = match method {
            Method::Both | Method::Mdns => {
                let mdns = Mdns::new()?;
                let events = mdns.browse()?;
                Some((mdns, events))
            }
            Method::Probe => None,
        };
        let prober = match method {
            Method::Both | Method::Probe => {
                let prober = Prober::new().await?;
                let (tx, rx) = watch::channel(RoundReport::default());
                let task = prober.spawn_rounds(PROBE_INTERVAL, tx);
                Some((prober, task, rx))
            }
            Method::Mdns => None,
        };
        Ok(Self {
            mdns,
            prober,
            peers: HashMap::new(),
            pending: VecDeque::new(),
            stats: Stats::default(),
        })
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
            let (announcement, addrs, via) = tokio::select! {
                event = recv_mdns(self.mdns.as_ref()) => {
                    match event? {
                        ServiceEvent::ServiceResolved(service) => match mdns::parse(&service) {
                            Ok((announcement, addrs)) => {
                                self.stats.mdns_records += 1;
                                (announcement, addrs, "mDNS")
                            }
                            Err(e) => {
                                debug!(fullname = %service.fullname, "ignoring record: {e:#}");
                                continue;
                            }
                        },
                        _ => continue,
                    }
                }
                (announcement, addr) = recv_probe(self.prober.as_ref()) => {
                    self.stats.probe_replies += 1;
                    (announcement, vec![addr], "probe")
                }
            };
            if let Some(peer) = self.merge(&announcement, addrs, via) {
                return Ok(peer);
            }
        }
    }

    /// Records an announcement; returns the peer if anything changed.
    fn merge(
        &mut self,
        announcement: &Announcement,
        addrs: Vec<SocketAddr>,
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
            }
        });
        let mut changed = peer.pairing != announcement.pairing || peer.label != label;
        peer.pairing = announcement.pairing;
        peer.label = label;
        for addr in addrs {
            changed |= peer.addrs.insert(addr);
        }
        changed.then(|| peer.clone())
    }

    /// Where we looked, for "nothing found" errors.
    pub fn describe(&self) -> String {
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
        let mut text = parts.join("; ");
        if self.prober.is_none() && self.stats.mdns_records == 0 {
            text.push_str(
                ". No DNS-SD records arrived at all: this machine's firewall probably drops mDNS \
                 replies (UDP 5353). Use the default discovery, which also probes",
            );
        }
        text
    }

    pub fn shutdown(self) {
        if let Some((mdns, _)) = self.mdns {
            mdns.shutdown();
        }
        if let Some((_, task, _)) = self.prober {
            task.abort();
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
) -> (Announcement, SocketAddr) {
    match prober {
        Some((prober, _, _)) => prober.recv().await,
        None => std::future::pending().await,
    }
}

/// Addresses worth dialing or probing.
pub fn is_usable(ip: IpAddr) -> bool {
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || matches!(ip, IpAddr::V4(v4) if v4.is_broadcast()))
}
