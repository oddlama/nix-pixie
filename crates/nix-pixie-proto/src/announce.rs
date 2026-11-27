//! What a target tells controllers about itself, so a controller can find it
//! and dial it. Only targets announce. The controller never listens, so the
//! operator's machine needs no open ports.
//!
//! The same [`Announcement`] travels three ways:
//!
//! * **DNS-SD** TXT records on `_nix-pixie._udp`. This works when the
//!   controller's firewall lets mDNS replies in. Many don't: replies to a
//!   multicast query come from the responder's unicast address, which
//!   connection tracking can't match to the query, so a stateful firewall
//!   drops them.
//! * **Probe replies** on UDP [`PROBE_PORT`]. The controller sends probes and
//!   reads the replies on the same socket. A reply to a *unicast* probe
//!   matches the probe's connection-tracking entry, so it passes any stateful
//!   firewall. The controller learns which addresses to probe from its
//!   neighbour table (see the binary's `net` module).
//! * **Through a relay**, to controllers on other networks (see
//!   [`crate::rendezvous`]).
//!
//! Everything here comes from the network and is untrusted. The endpoint id
//! is only a claim until iroh authenticates it when dialing.

use serde::{Deserialize, Serialize};

use crate::PROTOCOL_VERSION;
use crate::code::NameplateSpace;
use crate::hex;

/// UDP port where targets answer probes.
pub const PROBE_PORT: u16 = 47_012;
/// Longest label, in characters.
pub const MAX_LABEL: usize = 63;

/// How the target wants to pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pairing {
    /// The target shows a code starting with this nameplate.
    ShowsCode { nameplate: u32 },
    /// The operator typed a controller's code on the target.
    TypedControllerCode,
}

/// What a waiting target publishes, via DNS-SD TXT records, in probe replies
/// and at its rendezvous: enough to dial it and to tell which code it is
/// waiting for.
/// Unauthenticated; pairing proves who is behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Announcement {
    pub version: u32,
    /// Raw ed25519 public key of the target's iroh endpoint.
    pub id: [u8; 32],
    /// UDP port of the target's iroh endpoint.
    pub port: u16,
    pub pairing: Pairing,
    /// Human-readable name, e.g. the hostname. Sanitise before display.
    pub label: String,
}

/// Why an announcement from the network was ignored.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AnnounceError {
    #[error("missing `{0}`")]
    Missing(&'static str),
    #[error("invalid `{0}`")]
    Invalid(&'static str),
    #[error(
        "it speaks nix-pixie protocol {0}, this one {PROTOCOL_VERSION}: use an installer of the \
         same nix-pixie version as the controller"
    )]
    Version(u32),
}

impl Announcement {
    /// Builds an announcement for this protocol version, truncating the label.
    #[must_use]
    pub fn new(id: [u8; 32], port: u16, pairing: Pairing, label: &str) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            port,
            pairing,
            label: label.chars().take(MAX_LABEL).collect(),
        }
    }

    /// TXT record entries. The port goes into the SRV record instead.
    #[must_use]
    pub fn txt(&self) -> Vec<(&'static str, String)> {
        let mut txt = vec![
            ("v", self.version.to_string()),
            ("id", hex::encode(&self.id)),
            ("label", self.label.clone()),
        ];
        match self.pairing {
            Pairing::ShowsCode { nameplate } => {
                txt.push(("pair", "show".to_string()));
                txt.push(("np", nameplate.to_string()));
            }
            Pairing::TypedControllerCode => txt.push(("pair", "typed".to_string())),
        }
        txt
    }

    /// Parses TXT record entries (looked up through `get`) plus the SRV port.
    ///
    /// # Errors
    ///
    /// If a field is missing or malformed, or the version is not ours.
    pub fn from_txt<'a>(
        get: impl Fn(&str) -> Option<&'a str>,
        port: u16,
    ) -> Result<Self, AnnounceError> {
        let field = |key: &'static str| get(key).ok_or(AnnounceError::Missing(key));
        let version = field("v")?
            .parse()
            .map_err(|_| AnnounceError::Invalid("v"))?;
        let id = hex::decode(field("id")?)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(AnnounceError::Invalid("id"))?;
        let pairing = match field("pair")? {
            "show" => Pairing::ShowsCode {
                nameplate: field("np")?
                    .parse()
                    .map_err(|_| AnnounceError::Invalid("np"))?,
            },
            "typed" => Pairing::TypedControllerCode,
            _ => return Err(AnnounceError::Invalid("pair")),
        };
        Self {
            version,
            id,
            port,
            pairing,
            label: get("label").unwrap_or("").to_string(),
        }
        .validated()
    }

    pub(crate) fn validated(self) -> Result<Self, AnnounceError> {
        if self.version != PROTOCOL_VERSION {
            return Err(AnnounceError::Version(self.version));
        }
        if let Pairing::ShowsCode { nameplate } = self.pairing
            && NameplateSpace::of(nameplate).is_none()
        {
            return Err(AnnounceError::Invalid("np"));
        }
        if self.label.chars().count() > MAX_LABEL {
            return Err(AnnounceError::Invalid("label"));
        }
        Ok(self)
    }
}

/// Random per-discovery value that replies must echo.
pub type Nonce = [u8; 16];

/// A fresh random probe nonce.
#[must_use]
pub fn new_nonce() -> Nonce {
    rand::random()
}

/// Probes are padded to this size. Replies are always smaller, so a forged
/// source address can't turn a target into a traffic amplifier.
pub const PROBE_LEN: usize = 512;
const PROBE_MAGIC: &[u8; 16] = b"nix-pixie/probe1";
const REPLY_MAGIC: &[u8; 16] = b"nix-pixie/reply1";

/// A probe carrying `nonce`, padded to [`PROBE_LEN`].
#[must_use]
pub fn encode_probe(nonce: &Nonce) -> Vec<u8> {
    let mut probe = Vec::with_capacity(PROBE_LEN);
    probe.extend_from_slice(PROBE_MAGIC);
    probe.extend_from_slice(nonce);
    probe.resize(PROBE_LEN, 0);
    probe
}

/// The probe's nonce, if `datagram` is a well-formed probe.
#[must_use]
pub fn decode_probe(datagram: &[u8]) -> Option<Nonce> {
    if datagram.len() != PROBE_LEN || !datagram.starts_with(PROBE_MAGIC) {
        return None;
    }
    datagram.get(16..32)?.try_into().ok()
}

/// The reply to a probe with `nonce`.
///
/// # Errors
///
/// If the announcement doesn't serialise, which only a bug can cause.
pub fn encode_reply(nonce: &Nonce, announcement: &Announcement) -> postcard::Result<Vec<u8>> {
    let mut reply = Vec::with_capacity(PROBE_LEN);
    reply.extend_from_slice(REPLY_MAGIC);
    reply.extend_from_slice(nonce);
    let reply = postcard::to_extend(announcement, reply)?;
    debug_assert!(reply.len() < PROBE_LEN, "reply of {} bytes", reply.len());
    Ok(reply)
}

/// The announcement in a reply to our probe with `nonce`.
///
/// # Errors
///
/// If the datagram is not a reply to this probe, or its announcement is
/// malformed or of another protocol version.
pub fn decode_reply(datagram: &[u8], nonce: &Nonce) -> Result<Announcement, AnnounceError> {
    let Some((header, body)) = datagram.split_at_checked(32) else {
        return Err(AnnounceError::Invalid("reply"));
    };
    if !header.starts_with(REPLY_MAGIC) {
        return Err(AnnounceError::Invalid("reply"));
    }
    if header.get(16..) != Some(nonce.as_slice()) {
        return Err(AnnounceError::Invalid("nonce"));
    }
    postcard::from_bytes::<Announcement>(body)
        .map_err(|_| AnnounceError::Invalid("reply"))?
        .validated()
}
