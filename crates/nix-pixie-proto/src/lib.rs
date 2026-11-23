//! Wire protocol shared by the nix-pixie controller and agent.
//!
//! Transport-agnostic: everything here works on any `AsyncRead + AsyncWrite`,
//! so the tests run over in-memory pipes. The binary wires it to iroh.

pub mod announce;
pub mod code;
pub mod ctrl;
pub mod frame;
pub mod hex;
pub mod inventory;
pub mod pair;
pub mod rendezvous;

/// Wire protocol version. Advertised in DNS-SD TXT records and checked in the
/// handshake. The controller supports this version and the previous one, so
/// older installer images keep working.
pub const PROTOCOL_VERSION: u32 = 1;

/// DNS-SD service type that controllers and targets advertise on the LAN.
pub const DNS_SD_SERVICE: &str = "_nix-pixie._udp.local.";

/// UDP port for the iroh endpoint unless configured otherwise. Fixed so that
/// firewalls can allow it; the installer image opens it.
pub const DEFAULT_PORT: u16 = 47_011;

/// iroh ALPN for control messages, starting with pairing.
pub const ALPN_CTRL: &[u8] = b"nix-pixie/ctrl/1";
/// iroh ALPN of a target's rendezvous endpoint (see [`rendezvous`]).
pub const ALPN_RENDEZVOUS: &[u8] = b"nix-pixie/rendezvous/1";
/// iroh ALPN for the byte stream forwarded to the target's store daemon.
pub const ALPN_STORE: &[u8] = b"nix-pixie/store/1";
/// iroh ALPN for log and progress streaming.
pub const ALPN_LOG: &[u8] = b"nix-pixie/log/1";
