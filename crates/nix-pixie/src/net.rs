//! iroh endpoint and pairing over it.
//!
//! Targets are found by `discovery` and dialed at the addresses it reports,
//! and through relays if they answered at their rendezvous (see
//! `rendezvous`). No iroh address lookup services are used (those would
//! publish endpoints on n0's DNS servers). iroh authenticates the dialed
//! endpoint id, so a forged announcement can at most point us at the wrong
//! machine, and pairing then fails.
//!
//! The controller always dials and the agent always accepts. The operator's
//! machine never listens for anything.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use iroh::endpoint::{BindOpts, Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl, SecretKey, TransportAddr};
use nix_pixie_proto::ALPN_CTRL;
use nix_pixie_proto::code::Code;
use nix_pixie_proto::pair::{PairError, PairFailure, PairParams, Role, Side, pair};
use tokio::io::Join;

use crate::discovery::Peer;
use crate::dns;

/// How long to wait for a QUIC handshake with a discovered peer.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound for the whole pairing exchange.
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(20);

/// A bidirectional QUIC stream as one `AsyncRead + AsyncWrite`.
pub type Stream = Join<RecvStream, SendStream>;

/// The relay servers an endpoint uses. A relay forwards (encrypted) packets
/// between endpoints that connected to it, so a target behind NAT or a
/// firewall can be reached, and helps the two find a direct path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relays {
    /// None: only local networks and addresses the controller can reach.
    Disabled,
    /// The public relays of n0, the makers of iroh.
    N0,
    /// Relays the user chose, e.g. their own `iroh-relay`.
    Custom(BTreeSet<RelayUrl>),
}

impl Relays {
    /// From the command line: `--relay URL` (repeatable) and `--no-relay`.
    pub fn from_args(urls: &[String], disabled: bool) -> Result<Self> {
        if disabled {
            return Ok(Self::Disabled);
        }
        if urls.is_empty() {
            return Ok(Self::N0);
        }
        let mut relays = BTreeSet::new();
        for url in urls {
            let relay: RelayUrl = url
                .parse()
                .with_context(|| format!("`{url}` is not a relay URL"))?;
            if !matches!(relay.scheme(), "https" | "http") || relay.host_str().is_none() {
                bail!("`{url}` is not a relay URL: expected https://HOST[:PORT]");
            }
            relays.insert(relay);
        }
        Ok(Self::Custom(relays))
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Every relay a peer using the same setting may be connected to.
    pub fn urls(&self) -> BTreeSet<RelayUrl> {
        self.mode().relay_map().urls()
    }

    pub(crate) fn mode(&self) -> RelayMode {
        match self {
            Self::Disabled => RelayMode::Disabled,
            Self::N0 => RelayMode::Default,
            Self::Custom(urls) => RelayMode::custom(urls.iter().cloned()),
        }
    }
}

impl std::fmt::Display for Relays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "no relay"),
            Self::N0 => write!(f, "n0's public relays"),
            Self::Custom(urls) => {
                let urls: Vec<String> = urls.iter().map(ToString::to_string).collect();
                write!(f, "{}", urls.join(", "))
            }
        }
    }
}

/// Binds an iroh endpoint on `port` (0 = any) for IPv4 and, if available,
/// IPv6.
pub async fn bind(secret: SecretKey, port: u16, relays: &Relays) -> Result<Endpoint> {
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![ALPN_CTRL.to_vec()])
        .relay_mode(relays.mode())
        .dns_resolver(dns::resolver())
        .clear_ip_transports()
        .bind_addr((Ipv4Addr::UNSPECIFIED, port))?
        .bind_addr_with_opts(
            (Ipv6Addr::UNSPECIFIED, port),
            BindOpts::default().set_is_required(false),
        )?
        .bind()
        .await
        .with_context(|| format!("binding UDP port {port}"))?;
    Ok(endpoint)
}

/// The port the endpoint's IPv4 socket is bound to.
pub fn bound_port(endpoint: &Endpoint) -> Result<u16> {
    endpoint
        .bound_sockets()
        .iter()
        .find(|a| a.is_ipv4())
        .map(SocketAddr::port)
        .context("endpoint has no IPv4 socket")
}

/// Why dialing a target failed.
#[derive(Debug, thiserror::Error)]
pub enum DialError {
    /// Couldn't reach it. Worth retrying if new addresses show up.
    #[error("connecting: {0:#}")]
    Connect(anyhow::Error),
    #[error(transparent)]
    Pair(#[from] PairFailure),
}

/// Dials a discovered target and pairs with it.
pub async fn dial_and_pair(
    endpoint: &Endpoint,
    peer: &Peer,
    side: Side,
    code: &Code,
) -> Result<(Connection, Stream), DialError> {
    let addr = EndpointAddr::from_parts(peer.id, routes(peer));
    let conn = tokio::time::timeout(CONNECT_TIMEOUT, endpoint.connect(addr, ALPN_CTRL))
        .await
        .map_err(|_| DialError::Connect(anyhow::anyhow!("timed out")))?
        .map_err(|e| DialError::Connect(e.into()))?;
    let (send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| DialError::Connect(e.into()))?;
    let mut stream = tokio::io::join(recv, send);
    run_pairing(endpoint, &conn, &mut stream, Role::Controller, side, code).await?;
    Ok((conn, stream))
}

/// The addresses to dial: everything except IPv6 link-local, unless that's all
/// there is. Link-local is the fallback for direct cables. It depends on
/// scope ids (which stock iroh drops, iroh issue 4584), and `NetworkManager` briefly
/// removes it whenever it gives up on DHCP on that link.
pub fn dial_addrs(addrs: &BTreeSet<SocketAddr>) -> Vec<SocketAddr> {
    let is_link_local =
        |a: &SocketAddr| matches!(a, SocketAddr::V6(v6) if v6.ip().is_unicast_link_local());
    let routable: Vec<SocketAddr> = addrs
        .iter()
        .copied()
        .filter(|a| !is_link_local(a))
        .collect();
    if routable.is_empty() {
        addrs.iter().copied().collect()
    } else {
        routable
    }
}

/// Every way to reach `peer`: its addresses worth dialing, and the relays
/// it may be connected to.
pub fn routes(peer: &Peer) -> Vec<TransportAddr> {
    dial_addrs(&peer.addrs)
        .into_iter()
        .map(TransportAddr::Ip)
        .chain(peer.relays.iter().cloned().map(TransportAddr::Relay))
        .collect()
}

/// The addresses of this machine's network interfaces, except loopback ones.
pub fn own_addresses() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .map(|interface| interface.ip())
        .collect()
}

/// Whether `addr`, where a connection to a machine goes, is one of the
/// addresses `own` that machine has itself. If not, something in between
/// translates addresses, and other ports of `addr` lead elsewhere.
pub fn is_own(addr: SocketAddr, own: &[IpAddr]) -> bool {
    let ip = addr.ip().to_canonical();
    own.iter().any(|own| own.to_canonical() == ip)
}

/// How data currently travels on `conn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Straight to this address of the peer.
    Direct(SocketAddr),
    /// Through this relay: slow, and at the relay operator's expense.
    Relayed(RelayUrl),
    /// No path is selected (yet).
    Unknown,
}

impl Route {
    /// `direct`, `relay` or `unknown`, for machine-readable output.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Direct(_) => "direct",
            Self::Relayed(_) => "relay",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct(addr) => write!(f, "{addr}"),
            Self::Relayed(url) => write!(f, "relayed through {url}"),
            Self::Unknown => write!(f, "no path"),
        }
    }
}

/// The path `conn` sends data on right now. A connection that started
/// through a relay moves to a direct path once the two endpoints found one.
pub fn route(conn: &Connection) -> Route {
    let paths = conn.paths();
    match paths
        .iter()
        .find(iroh::endpoint::Path::is_selected)
        .map(|p| p.remote_addr().clone())
    {
        Some(TransportAddr::Ip(addr)) => Route::Direct(addr),
        Some(TransportAddr::Relay(url)) => Route::Relayed(url),
        _ => Route::Unknown,
    }
}

/// Accepts the control stream of an incoming connection and pairs.
pub async fn accept_and_pair(
    endpoint: &Endpoint,
    conn: &Connection,
    side: Side,
    code: &Code,
) -> Result<Stream, PairFailure> {
    let (send, recv) = match tokio::time::timeout(PAIR_TIMEOUT, conn.accept_bi()).await {
        Ok(Ok(streams)) => streams,
        Ok(Err(e)) => return Err(io_failure(std::io::Error::other(e))),
        Err(_) => {
            return Err(PairFailure {
                error: PairError::Timeout,
                guess_spent: false,
            });
        }
    };
    let mut stream = tokio::io::join(recv, send);
    run_pairing(endpoint, conn, &mut stream, Role::Target, side, code).await?;
    Ok(stream)
}

async fn run_pairing(
    endpoint: &Endpoint,
    conn: &Connection,
    stream: &mut Stream,
    role: Role,
    side: Side,
    code: &Code,
) -> Result<(), PairFailure> {
    let params = PairParams {
        role,
        side,
        code,
        // Both ids come from iroh's handshake, never from the peer's messages.
        local_id: *endpoint.id().as_bytes(),
        remote_id: *conn.remote_id().as_bytes(),
    };
    pair(stream, params, PAIR_TIMEOUT).await
}

fn io_failure(e: std::io::Error) -> PairFailure {
    PairFailure {
        error: PairError::Frame(e.into()),
        guess_spent: false,
    }
}

/// Ends our side of `stream` and waits (briefly) until the peer has
/// acknowledged everything we sent. Closing the connection right after the
/// last write would otherwise discard data still in flight, e.g. a final
/// `Reject`.
pub async fn finish(stream: &mut Stream) {
    let send = stream.writer_mut();
    if send.finish().is_ok()
        && tokio::time::timeout(Duration::from_secs(5), send.stopped())
            .await
            .is_err()
    {
        tracing::debug!("peer did not acknowledge the end of the stream");
    }
}

/// Makes untrusted text safe to log and display: printable characters only,
/// at most `max` of them. Besides control characters this drops the
/// invisible formatting characters (bidi overrides, zero-width ones) that
/// can make text display as something it isn't.
pub fn sanitise(text: &str, max: usize) -> String {
    text.chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|&c| !c.is_control() && !is_invisible_format(c))
        .take(max)
        .collect()
}

/// Unicode format characters that reorder or hide text.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// The machine's hostname, for labels.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| sanitise(h.trim(), nix_pixie_proto::announce::MAX_LABEL))
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
