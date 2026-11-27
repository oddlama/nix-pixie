//! SSH host keys on the controller: checking the key a target generated
//! (pull mode), and after the first boot, asking the installed system for
//! its key and comparing.

use std::collections::BTreeSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{debug, info};

/// Path to `ssh-keyscan`; the Nix build pins it to OpenSSH from the store.
const SSH_KEYSCAN: &str = match option_env!("NIX_PIXIE_SSH_KEYSCAN") {
    Some(path) => path,
    None => "ssh-keyscan",
};
const KIND: &str = "ssh-ed25519";
/// Every ed25519 key blob starts like this in base64: the length-prefixed
/// type name and the length of the 32-byte key.
const BLOB_PREFIX: &str = "AAAAC3NzaC1lZDI1NTE5AAAAI";
const BLOB_LEN: usize = 68;

/// An ed25519 public key in OpenSSH's format, without the comment.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PublicKey {
    blob: String,
}

impl PublicKey {
    /// Parses `ssh-ed25519 <base64> [comment]`. Strict: this comes from
    /// the target or the network.
    pub fn parse(line: &str) -> Result<Self> {
        let mut fields = line.split_whitespace();
        let (Some(kind), Some(blob)) = (fields.next(), fields.next()) else {
            bail!("not an OpenSSH public key");
        };
        if kind != KIND {
            bail!("a {kind} key, expected {KIND}");
        }
        let base64 = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=');
        if blob.len() != BLOB_LEN || !blob.starts_with(BLOB_PREFIX) || !blob.bytes().all(base64) {
            bail!("a malformed {KIND} key");
        }
        Ok(Self { blob: blob.into() })
    }

    /// The key, shortened to its last 12 characters for a glance:
    /// `ssh-ed25519 …LVSO3OiU+MZo`.
    pub fn short(&self) -> String {
        let tail = self
            .blob
            .char_indices()
            .nth_back(11)
            .map_or("", |(i, _)| self.blob.get(i..).unwrap_or_default());
        format!("{KIND} …{tail}")
    }

    /// The line for a `.pub` file, with `comment` if it is a plain one.
    pub fn to_line(&self, comment: Option<&str>) -> String {
        let plain = |c: &str| {
            !c.is_empty()
                && c.len() <= 128
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'@' | b'.' | b'_' | b'-'))
        };
        match comment.filter(|c| plain(c)) {
            Some(comment) => format!("{self} {comment}\n"),
            None => format!("{self}\n"),
        }
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{KIND} {}", self.blob)
    }
}

/// The ed25519 keys in `ssh-keyscan` output (`<host> <type> <blob>` lines;
/// comments and other key types are skipped).
pub fn parse_keyscan(output: &str) -> BTreeSet<PublicKey> {
    output
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let (_host, key) = line.split_once(char::is_whitespace)?;
            PublicKey::parse(key).ok()
        })
        .collect()
}

/// The host argument for `ssh-keyscan`: link-local IPv6 with its scope.
fn host_arg(addr: SocketAddr) -> String {
    match addr {
        SocketAddr::V6(v6) if v6.scope_id() != 0 => format!("{}%{}", v6.ip(), v6.scope_id()),
        other => other.ip().to_string(),
    }
}

async fn scan(addr: SocketAddr, port: u16) -> Result<BTreeSet<PublicKey>> {
    let output = tokio::process::Command::new(SSH_KEYSCAN)
        .args(["-T", "3", "-t", "ed25519", "-p", &port.to_string()])
        .arg(host_arg(addr))
        .output()
        .await
        .context("running ssh-keyscan (is OpenSSH installed?)")?;
    Ok(parse_keyscan(&String::from_utf8_lossy(&output.stdout)))
}

/// What the installed system answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub key: PublicKey,
    pub addr: IpAddr,
}

/// What one host's answer means while waiting for the installed system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing yet, or only the installer's own key.
    Wait,
    /// The installed system's key: the expected one, or with none
    /// expected, the first that isn't the installer's.
    Seen(PublicKey),
    /// A key that is neither: another machine, or an attacker.
    Mismatch(PublicKey),
}

/// Judges the keys one host answered with.
pub fn judge(
    keys: &BTreeSet<PublicKey>,
    expected: Option<&PublicKey>,
    ignore: Option<&PublicKey>,
) -> Verdict {
    let mut relevant = keys.iter().filter(|k| Some(*k) != ignore);
    match (expected, relevant.next()) {
        (_, None) => Verdict::Wait,
        (Some(expected), Some(_)) if keys.contains(expected) => Verdict::Seen(expected.clone()),
        (Some(_), Some(other)) => Verdict::Mismatch(other.clone()),
        (None, Some(key)) => Verdict::Seen(key.clone()),
    }
}

/// The hosts to ask: one per IP (and IPv6 scope) in `addrs`, whose ports
/// are the agent's, not SSH's.
pub fn hosts(addrs: &[SocketAddr]) -> BTreeSet<SocketAddr> {
    addrs
        .iter()
        .map(|a| {
            let mut host = *a;
            host.set_port(0);
            host
        })
        .collect()
}

/// The link-local address a NIC makes from its MAC (EUI-64), as the kernel
/// and systemd-networkd do by default (`NetworkManager` doesn't).
#[must_use]
pub fn eui64_link_local(mac: &str) -> Option<Ipv6Addr> {
    let bytes: Vec<u8> = mac
        .split(':')
        .map(|b| u8::from_str_radix(b, 16))
        .collect::<Result<_, _>>()
        .ok()?;
    let [oui0, oui1, oui2, nic0, nic1, nic2]: [u8; 6] = bytes.try_into().ok()?;
    Some(Ipv6Addr::new(
        0xfe80,
        0,
        0,
        0,
        u16::from_be_bytes([oui0 ^ 0x02, oui1]),
        u16::from_be_bytes([oui2, 0xff]),
        u16::from_be_bytes([0xfe, nic0]),
        u16::from_be_bytes([nic1, nic2]),
    ))
}

/// The links the controller shares with a target found at `addrs`, by
/// interface index: those its link-local addresses are on, and those of
/// the controller's interfaces `own` (index, address, netmask) whose IPv4
/// network holds one of its IPv4 addresses.
#[must_use]
pub fn shared_links(addrs: &[SocketAddr], own: &[(u32, Ipv4Addr, Ipv4Addr)]) -> BTreeSet<u32> {
    let mut links = BTreeSet::new();
    for addr in addrs {
        match addr {
            SocketAddr::V6(v6) if v6.ip().is_unicast_link_local() && v6.scope_id() != 0 => {
                links.insert(v6.scope_id());
            }
            SocketAddr::V4(v4) => {
                let ip = u32::from(*v4.ip());
                for &(index, own_ip, mask) in own {
                    let mask = u32::from(mask);
                    if mask != 0 && ip & mask == u32::from(own_ip) & mask {
                        links.insert(index);
                    }
                }
            }
            SocketAddr::V6(_) => {}
        }
    }
    links
}

/// Where a target with NICs of `macs` may answer on `links` by the
/// link-local addresses it makes from them: for an installed system that
/// gets no other address, or none the controller knows.
#[must_use]
pub fn from_macs(macs: &[String], links: &BTreeSet<u32>) -> Vec<SocketAddr> {
    macs.iter()
        .filter_map(|mac| eui64_link_local(mac))
        .flat_map(|ip| {
            links
                .iter()
                .map(move |&scope| SocketAddr::V6(SocketAddrV6::new(ip, 0, 0, scope)))
        })
        .collect()
}

/// `hosts` for people: link-local IPv6 with its scope.
pub fn describe_hosts(hosts: &BTreeSet<SocketAddr>) -> String {
    hosts
        .iter()
        .map(|a| host_arg(*a))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Asks every host for its SSH host key once. `Some` once one answers with
/// a key other than `ignore` (the installer's own, while the target is
/// still going down). With an `expected` key, any other key is a mismatch:
/// it isn't the installer's, so it is another machine or an attacker.
pub async fn check(
    hosts: &BTreeSet<SocketAddr>,
    port: u16,
    expected: Option<&PublicKey>,
    ignore: Option<&PublicKey>,
) -> Result<Option<Seen>> {
    // All at once, since most don't answer and each takes its timeout to
    // say so; judged in order.
    let mut scans = tokio::task::JoinSet::new();
    for &addr in hosts {
        scans.spawn(async move { (addr, scan(addr, port).await) });
    }
    let mut answers: Vec<_> = scans.join_all().await;
    answers.sort_by_key(|(addr, _)| *addr);
    for (addr, keys) in answers {
        let keys = keys?;
        debug!(%addr, keys = keys.len(), "ssh-keyscan");
        match judge(&keys, expected, ignore) {
            Verdict::Wait => {}
            Verdict::Seen(key) => {
                info!(%addr, "host key seen");
                return Ok(Some(Seen {
                    key,
                    addr: addr.ip(),
                }));
            }
            Verdict::Mismatch(other) => bail!(
                "HOST KEY MISMATCH: {} answered with {other}, not with the key the install put \
                 there. It is another machine, or someone is impersonating this one",
                host_arg(addr)
            ),
        }
    }
    Ok(None)
}

/// When to ask a booting machine for its host key: every `interval`, and
/// once more at the deadline. An operator who says the machine is up gets a
/// check right away, from which the interval counts anew. A fixed pace
/// instead of retrying in a loop: a machine that is powered off for a while
/// isn't asked hundreds of times, and there is no backoff to wait out once
/// it is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    interval: Duration,
    deadline: Instant,
    next: Option<Instant>,
}

impl Cadence {
    /// From `start` on, for up to `timeout`. A zero `interval` counts as a
    /// second.
    pub fn new(start: Instant, interval: Duration, timeout: Duration) -> Self {
        let mut cadence = Self {
            interval: interval.max(Duration::from_secs(1)),
            deadline: start + timeout,
            next: None,
        };
        cadence.schedule(start);
        cadence
    }

    fn schedule(&mut self, from: Instant) {
        self.next = (from < self.deadline).then(|| (from + self.interval).min(self.deadline));
    }

    /// When to check next; `None` once the check at the deadline is done.
    pub fn next(&self) -> Option<Instant> {
        self.next
    }

    /// A check was done at `at`.
    pub fn checked(&mut self, at: Instant) {
        self.schedule(at);
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}
