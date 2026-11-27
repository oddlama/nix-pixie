//! Store copies over another network than the session's (`--bulk-via`): the
//! session over a USB cable, the copy over Ethernet, say. The target opens a
//! bulk endpoint at one of its addresses (`OpenBulk`), bound to that address
//! alone so that the copy can't wander back to the session's path; the
//! controller connects to it, and the link's store copies go there. If that
//! path breaks, they go over the session's again.

use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use iroh::{EndpointAddr, EndpointId, TransportAddr};
use nix_pixie_proto::ALPN_STORE;
use nix_pixie_proto::ctrl::{ControllerMsg, TargetMsg};
use nix_pixie_proto::inventory::Nic;
use tracing::{debug, info, warn};

use crate::link::Link;
use crate::net::{self, Route};

/// How long the target may take to open its bulk endpoint.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long connecting to it may take: it is near, or it isn't the way.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// For the target to list its addresses.
const ADDRESSES_TIMEOUT: Duration = Duration::from_secs(10);

/// Where store copies go (`--bulk-via`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Via {
    /// Over the session's connection.
    #[default]
    Session,
    /// To the first of the target's addresses that works, of its NICs other
    /// than the session's.
    Auto,
    /// To this address of the target's.
    Addr(IpAddr),
}

impl FromStr for Via {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "session" => Ok(Self::Session),
            "auto" => Ok(Self::Auto),
            addr => addr.parse().map(Self::Addr).map_err(|_| {
                format!("{addr:?}: `auto`, `session`, or one of the target's IP addresses")
            }),
        }
    }
}

static VIA: OnceLock<Via> = OnceLock::new();

/// Sets where store copies go, for the rest of the run.
pub fn set_via(via: Via) {
    let _ = VIA.set(via);
}

fn via() -> Via {
    VIA.get().copied().unwrap_or_default()
}

/// For `auto`: the target's addresses on other NICs than the one the
/// session goes to (`session`, its address there; `None` if it goes
/// through a relay), IPv4 first. Not link-local ones: which of the
/// controller's links they are on is anybody's guess.
#[must_use]
pub fn candidates(nics: &[Nic], session: Option<IpAddr>) -> Vec<IpAddr> {
    let on_session = |nic: &&Nic| session.is_some_and(|ip| nic.addrs.contains(&ip));
    let mut addrs: Vec<IpAddr> = nics
        .iter()
        .filter(|nic| nic.link_up && !on_session(nic))
        .flat_map(|nic| nic.addrs.iter().copied())
        .filter(|ip| match ip {
            IpAddr::V4(v4) => !(v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()),
            IpAddr::V6(v6) => {
                !(v6.is_loopback() || v6.is_unicast_link_local() || v6.is_unspecified())
            }
        })
        .collect();
    addrs.sort_by_key(|ip| (ip.is_ipv6(), *ip));
    addrs.dedup();
    addrs
}

/// Opens the bulk path `--bulk-via` asks for, if it can: the link's store
/// copies go there from now on. Returns its address, for the report; `None`
/// over the session's connection.
pub async fn open(link: &Link) -> Result<Option<IpAddr>> {
    let addrs = match via() {
        Via::Session => return Ok(None),
        Via::Addr(addr) => vec![addr],
        Via::Auto => {
            let session = match net::route(&link.connection()) {
                Route::Direct(addr) => Some(addr.ip().to_canonical()),
                Route::Relayed(_) | Route::Unknown => None,
            };
            let found = candidates(&target_nics(link).await?, session);
            if found.is_empty() {
                info!("the target has no address on another network: copying over the session's");
                return Ok(None);
            }
            found
        }
    };
    for addr in addrs {
        match try_addr(link, addr).await {
            Ok(()) => {
                info!(%addr, "copying to the target over {addr}, the session's path aside");
                return Ok(Some(addr));
            }
            Err(e) => warn!(%addr, "no bulk path to the target there: {e:#}"),
        }
    }
    info!("copying over the session's path");
    Ok(None)
}

/// Has the target open its bulk endpoint at `addr`, and connects to it.
async fn try_addr(link: &Link, addr: IpAddr) -> Result<()> {
    let reply = link
        .request(
            &ControllerMsg::OpenBulk { addr },
            OPEN_TIMEOUT,
            "the target's bulk endpoint",
        )
        .await?;
    let (id, port) = match reply {
        TargetMsg::Bulk { id, port } => (id, port),
        TargetMsg::BulkFailed { error } => bail!("{error}"),
        _ => bail!("unexpected reply to opening a bulk endpoint"),
    };
    let id = EndpointId::from_bytes(&id).context("the bulk endpoint's id")?;
    let to = EndpointAddr::from_parts(id, [TransportAddr::Ip(SocketAddr::new(addr, port))]);
    let conn = tokio::time::timeout(CONNECT_TIMEOUT, link.endpoint().connect(to, ALPN_STORE))
        .await
        .map_err(|_| anyhow!("no answer within {} s", CONNECT_TIMEOUT.as_secs()))??;
    debug!(route = %net::route(&conn), "bulk connection");
    link.set_bulk(conn);
    Ok(())
}

/// The target's physical NICs, with their addresses now.
async fn target_nics(link: &Link) -> Result<Vec<Nic>> {
    match link
        .request(
            &ControllerMsg::RequestAddresses,
            ADDRESSES_TIMEOUT,
            "the target's addresses",
        )
        .await?
    {
        TargetMsg::Addresses { nics, .. } => Ok(nics),
        _ => bail!("unexpected reply to asking for the target's addresses"),
    }
}
