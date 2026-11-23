//! iroh endpoint and pairing over it.
//!
//! v1 is LAN-only: no relays and no iroh address lookup services (which would
//! contact n0's DNS servers). Targets are found by `discovery` and dialed at
//! the addresses it reports. iroh authenticates the dialed endpoint id, so a
//! forged announcement can at most point us at the wrong machine, and pairing
//! then fails.
//!
//! The controller always dials and the agent always accepts. The operator's
//! machine never listens for anything.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result};
use iroh::endpoint::{BindOpts, Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, SecretKey, TransportAddr};
use nix_pixie_proto::ALPN_CTRL;
use nix_pixie_proto::code::Code;
use nix_pixie_proto::pair::{PairError, PairFailure, PairParams, Role, Side, pair};
use tokio::io::Join;

use crate::discovery::Peer;

/// How long to wait for a QUIC handshake with a discovered peer.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound for the whole pairing exchange.
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(20);

/// A bidirectional QUIC stream as one `AsyncRead + AsyncWrite`.
pub type Stream = Join<RecvStream, SendStream>;

/// Binds an iroh endpoint on `port` (0 = any) for IPv4 and, if available, IPv6.
pub async fn bind(secret: SecretKey, port: u16) -> Result<Endpoint> {
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(secret)
        .alpns(vec![ALPN_CTRL.to_vec()])
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
    let addr = EndpointAddr::from_parts(
        peer.id,
        dial_addrs(&peer.addrs).into_iter().map(TransportAddr::Ip),
    );
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
