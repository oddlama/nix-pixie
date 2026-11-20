//! DNS-SD on `_nix-pixie._udp`: targets advertise, controllers browse.
//!
//! On the controller this only works if the firewall lets mDNS replies in;
//! `probe` covers the case where it doesn't.

use std::net::{IpAddr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use anyhow::{Context, Result};
use iroh::EndpointId;
use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use nix_pixie_proto::DNS_SD_SERVICE;
use nix_pixie_proto::announce::{Announcement, Pairing};

pub struct Mdns {
    daemon: ServiceDaemon,
}

impl Mdns {
    pub fn new() -> Result<Self> {
        Ok(Self {
            daemon: ServiceDaemon::new().context("starting mDNS (is UDP port 5353 available?)")?,
        })
    }

    /// Publishes `announcement`. Returns the name to pass to [`Self::withdraw`].
    pub fn advertise(&self, announcement: &Announcement) -> Result<String> {
        // Unique per endpoint and code, so a rotated code shows up as a new
        // instance instead of an update that browsers may have cached.
        let id = nix_pixie_proto::hex::encode(announcement.id.get(..6).unwrap_or(&announcement.id));
        let instance = match announcement.pairing {
            Pairing::ShowsCode { nameplate } => format!("target-{id}-{nameplate}"),
            Pairing::TypedControllerCode => format!("target-{id}-typed"),
        };
        let txt = announcement.txt();
        let properties: Vec<(&str, &str)> = txt.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let host = format!("{instance}.local.");
        let info = ServiceInfo::new(
            DNS_SD_SERVICE,
            &instance,
            &host,
            (),
            announcement.port,
            &properties[..],
        )?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        self.daemon.register(info)?;
        Ok(fullname)
    }

    /// Withdraws an advertisement (sends goodbye packets).
    pub fn withdraw(&self, fullname: &str) {
        if let Err(e) = self.daemon.unregister(fullname) {
            tracing::warn!("withdrawing {fullname}: {e}");
        }
    }

    pub fn browse(&self) -> Result<mdns_sd::Receiver<ServiceEvent>> {
        Ok(self.daemon.browse(DNS_SD_SERVICE)?)
    }

    /// Stops the daemon, withdrawing everything we advertised.
    pub fn shutdown(self) {
        if let Ok(done) = self.daemon.shutdown() {
            // Give the goodbye packets a moment; don't hang on exit.
            let _ = done.recv_timeout(Duration::from_secs(1));
        }
    }
}

/// A target's announcement from a resolved service, plus its addresses.
pub fn parse(service: &ResolvedService) -> Result<(Announcement, Vec<SocketAddr>)> {
    let announcement =
        Announcement::from_txt(|k| service.get_property_val_str(k), service.get_port())?;
    let addrs: Vec<SocketAddr> = service
        .get_addresses()
        .iter()
        .filter_map(|ip| socket_addr(ip, service.get_port()))
        .collect();
    anyhow::ensure!(!addrs.is_empty(), "no usable addresses");
    // Validate the id early so bad records never reach the dialer.
    EndpointId::from_bytes(&announcement.id).context("invalid endpoint id")?;
    Ok((announcement, addrs))
}

fn socket_addr(ip: &ScopedIp, port: u16) -> Option<SocketAddr> {
    let addr = ip.to_ip_addr();
    if !crate::discovery::is_usable(addr) {
        return None;
    }
    Some(match (ip, addr) {
        // Link-local IPv6 needs the interface it was seen on.
        (ScopedIp::V6(v6), IpAddr::V6(a)) if a.is_unicast_link_local() => {
            SocketAddr::V6(SocketAddrV6::new(a, port, 0, v6.scope_id().index))
        }
        _ => SocketAddr::new(addr, port),
    })
}
