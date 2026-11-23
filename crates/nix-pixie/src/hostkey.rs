//! SSH host keys on the controller: checking the key a target generated
//! (pull mode), and after the first boot, asking the installed system for
//! its key and comparing.

use std::collections::BTreeSet;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

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
/// Between rounds of asking every address while the target boots.
const SCAN_INTERVAL: Duration = Duration::from_secs(2);

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

/// Asks every address in `addrs` for the SSH host key until one answers
/// with a key other than `ignore` (the installer's own, while the target
/// is still going down), or `timeout` passes. With an `expected` key, any
/// other key is a mismatch: it isn't the installer's, so it is another
/// machine or an attacker.
pub async fn wait_for(
    addrs: &[SocketAddr],
    port: u16,
    expected: Option<&PublicKey>,
    ignore: Option<&PublicKey>,
    timeout: Duration,
) -> Result<Seen> {
    // One per IP (and IPv6 scope): the ports in `addrs` are the agent's.
    let hosts: BTreeSet<SocketAddr> = addrs
        .iter()
        .map(|a| {
            let mut host = *a;
            host.set_port(0);
            host
        })
        .collect();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        for &addr in &hosts {
            let keys = scan(addr, port).await?;
            debug!(%addr, keys = keys.len(), "ssh-keyscan");
            match judge(&keys, expected, ignore) {
                Verdict::Wait => {}
                Verdict::Seen(key) => {
                    info!(%addr, "host key seen");
                    return Ok(Seen {
                        key,
                        addr: addr.ip(),
                    });
                }
                Verdict::Mismatch(other) => bail!(
                    "HOST KEY MISMATCH: {} answered with {other}, not with the key the \
                     install put there. It is another machine, or someone is impersonating \
                     this one",
                    host_arg(addr)
                ),
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let tried: Vec<String> = hosts.iter().map(|a| host_arg(*a)).collect();
            bail!(
                "no SSH answer from {} within {} s",
                tried.join(", "),
                timeout.as_secs()
            );
        }
        tokio::time::sleep(SCAN_INTERVAL).await;
    }
}
